//! Defer and Undefer command implementations.

use crate::cli::{DeferArgs, UndeferArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::Status;
use crate::storage::IssueUpdate;
use crate::util::id::{IdResolver, ResolverConfig, find_matching_ids};
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc};
use serde::Serialize;

/// Result of deferring a single issue.
#[derive(Debug, Serialize)]
pub struct DeferredIssue {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_until: Option<String>,
}

/// Issue that was skipped during defer.
#[derive(Debug, Serialize)]
pub struct SkippedIssue {
    pub id: String,
    pub reason: String,
}

/// JSON output for defer command.
#[derive(Debug, Serialize)]
pub struct DeferResult {
    pub deferred: Vec<DeferredIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedIssue>,
}

/// JSON output for undefer command.
#[derive(Debug, Serialize)]
pub struct UndeferResult {
    pub undeferred: Vec<DeferredIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedIssue>,
}

/// Parse a flexible time specification into a `DateTime`.
///
/// Supports:
/// - RFC3339: `2025-01-15T12:00:00Z`, `2025-01-15T12:00:00+00:00`
/// - Simple date: `2025-01-15` (defaults to 9:00 AM local time)
/// - Relative duration: `+1h`, `+2d`, `+1w`, `+30m`
/// - Keywords: `tomorrow`, `next-week`
fn parse_defer_time(s: &str) -> Result<DateTime<Utc>> {
    let s = s.trim();

    // Try RFC3339 first
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    // Try simple date (YYYY-MM-DD) - default to 9:00 AM local time
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let naive_dt = date.and_time(time);
        let local_dt = Local
            .from_local_datetime(&naive_dt)
            .single()
            .ok_or_else(|| BeadsError::validation("defer_until", "ambiguous local time"))?;
        return Ok(local_dt.with_timezone(&Utc));
    }

    // Try relative duration (+1h, +2d, +1w, +30m)
    if let Some(rest) = s.strip_prefix('+') {
        if let Some(unit_char) = rest.chars().last() {
            let amount_str = &rest[..rest.len() - 1];
            if let Ok(amount) = amount_str.parse::<i64>() {
                let duration = match unit_char {
                    'm' => Duration::minutes(amount),
                    'h' => Duration::hours(amount),
                    'd' => Duration::days(amount),
                    'w' => Duration::weeks(amount),
                    _ => {
                        return Err(BeadsError::validation(
                            "defer_until",
                            "invalid unit (use m, h, d, w)",
                        ));
                    }
                };
                return Ok(Utc::now() + duration);
            }
        }
    }

    // Try keywords
    let now = Local::now();
    match s.to_lowercase().as_str() {
        "tomorrow" => {
            let tomorrow = now.date_naive() + Duration::days(1);
            let time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
            let naive_dt = tomorrow.and_time(time);
            let local_dt = Local
                .from_local_datetime(&naive_dt)
                .single()
                .ok_or_else(|| BeadsError::validation("defer_until", "ambiguous local time"))?;
            Ok(local_dt.with_timezone(&Utc))
        }
        "next-week" | "nextweek" => {
            let next_week = now.date_naive() + Duration::weeks(1);
            let time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
            let naive_dt = next_week.and_time(time);
            let local_dt = Local
                .from_local_datetime(&naive_dt)
                .single()
                .ok_or_else(|| BeadsError::validation("defer_until", "ambiguous local time"))?;
            Ok(local_dt.with_timezone(&Utc))
        }
        _ => Err(BeadsError::validation(
            "defer_until",
            "invalid time format (try: +1h, +2d, tomorrow, next-week, or 2025-01-15)",
        )),
    }
}

/// Execute the defer command.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
pub fn execute_defer(args: &DeferArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let use_json = json || args.robot;

    tracing::info!("Executing defer command");

    if args.ids.is_empty() {
        return Err(BeadsError::validation(
            "ids",
            "at least one issue ID is required",
        ));
    }

    let beads_dir = config::discover_beads_dir(None)?;
    let (mut storage, _paths) =
        config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage), cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage.get_all_ids()?;

    // Parse defer_until if provided
    let defer_until = args
        .until
        .as_ref()
        .map(|s| parse_defer_time(s))
        .transpose()?;

    // Resolve all IDs
    let resolved_ids = resolver.resolve_all(
        &args.ids,
        |id| all_ids.iter().any(|existing| existing == id),
        |hash| find_matching_ids(&all_ids, hash),
    )?;

    let mut deferred_issues: Vec<DeferredIssue> = Vec::new();
    let mut skipped_issues: Vec<SkippedIssue> = Vec::new();

    for resolved in &resolved_ids {
        let id = &resolved.id;
        tracing::info!(id = %id, until = ?defer_until, "Deferring issue");

        // Get current issue
        let Some(issue) = storage.get_issue(id)? else {
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: "issue not found".to_string(),
            });
            continue;
        };

        // Check if already closed/tombstone
        if issue.status.is_terminal() {
            tracing::debug!(id = %id, status = ?issue.status, "Issue is terminal");
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: format!("cannot defer {} issue", issue.status.as_str()),
            });
            continue;
        }

        // Check if already deferred (with same time)
        if issue.status == Status::Deferred && issue.defer_until == defer_until {
            tracing::debug!(id = %id, "Issue already deferred with same time");
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: "already deferred".to_string(),
            });
            continue;
        }

        // Build update: set status=deferred, set defer_until
        let update = IssueUpdate {
            status: Some(Status::Deferred),
            defer_until: Some(defer_until),
            ..Default::default()
        };

        // Apply update
        storage.update_issue(id, &update, &actor)?;
        tracing::info!(id = %id, defer_until = ?defer_until, "Issue deferred");

        // Update last touched
        crate::util::set_last_touched_id(&beads_dir, id);

        deferred_issues.push(DeferredIssue {
            id: id.clone(),
            title: issue.title.clone(),
            status: "deferred".to_string(),
            defer_until: defer_until.map(|dt| dt.to_rfc3339()),
        });
    }

    // Output
    if use_json {
        let result = DeferResult {
            deferred: deferred_issues,
            skipped: skipped_issues,
        };
        let output = serde_json::to_string_pretty(&result).map_err(BeadsError::Json)?;
        println!("{output}");
    } else {
        for deferred in &deferred_issues {
            print!("\u{23f1} Deferred {}: {}", deferred.id, deferred.title);
            if let Some(ref until) = deferred.defer_until {
                println!(" (until {until})");
            } else {
                println!(" (indefinitely)");
            }
        }
        for skipped in &skipped_issues {
            println!("\u{2298} Skipped {}: {}", skipped.id, skipped.reason);
        }
        if deferred_issues.is_empty() && skipped_issues.is_empty() {
            println!("No issues to defer.");
        }
    }

    Ok(())
}

/// Execute the undefer command.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
pub fn execute_undefer(args: &UndeferArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let use_json = json || args.robot;

    tracing::info!("Executing undefer command");

    if args.ids.is_empty() {
        return Err(BeadsError::validation(
            "ids",
            "at least one issue ID is required",
        ));
    }

    let beads_dir = config::discover_beads_dir(None)?;
    let (mut storage, _paths) =
        config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage), cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage.get_all_ids()?;

    // Resolve all IDs
    let resolved_ids = resolver.resolve_all(
        &args.ids,
        |id| all_ids.iter().any(|existing| existing == id),
        |hash| find_matching_ids(&all_ids, hash),
    )?;

    let mut undeferred_issues: Vec<DeferredIssue> = Vec::new();
    let mut skipped_issues: Vec<SkippedIssue> = Vec::new();

    for resolved in &resolved_ids {
        let id = &resolved.id;
        tracing::info!(id = %id, "Undeferring issue");

        // Get current issue
        let Some(issue) = storage.get_issue(id)? else {
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: "issue not found".to_string(),
            });
            continue;
        };

        // Check if actually deferred
        if issue.status != Status::Deferred {
            tracing::debug!(id = %id, status = ?issue.status, "Issue is not deferred");
            skipped_issues.push(SkippedIssue {
                id: id.clone(),
                reason: format!("not deferred (status: {})", issue.status.as_str()),
            });
            continue;
        }

        // Build update: set status=open, clear defer_until
        let update = IssueUpdate {
            status: Some(Status::Open),
            defer_until: Some(None), // Clear defer_until
            ..Default::default()
        };

        // Apply update
        storage.update_issue(id, &update, &actor)?;
        tracing::info!(id = %id, "Issue undeferred");

        // Update last touched
        crate::util::set_last_touched_id(&beads_dir, id);

        undeferred_issues.push(DeferredIssue {
            id: id.clone(),
            title: issue.title.clone(),
            status: "open".to_string(),
            defer_until: None,
        });
    }

    // Rebuild blocked cache since undeferred issues may become blockers
    if !undeferred_issues.is_empty() {
        tracing::info!(
            "Rebuilding blocked cache after undeferring {} issues",
            undeferred_issues.len()
        );
        storage.rebuild_blocked_cache(true)?;
    }

    // Output
    if use_json {
        let result = UndeferResult {
            undeferred: undeferred_issues,
            skipped: skipped_issues,
        };
        let output = serde_json::to_string_pretty(&result).map_err(BeadsError::Json)?;
        println!("{output}");
    } else {
        for undeferred in &undeferred_issues {
            println!(
                "\u{2713} Undeferred {}: {} (now open)",
                undeferred.id, undeferred.title
            );
        }
        for skipped in &skipped_issues {
            println!("\u{2298} Skipped {}: {}", skipped.id, skipped.reason);
        }
        if undeferred_issues.is_empty() && skipped_issues.is_empty() {
            println!("No issues to undefer.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands;
    use crate::config::CliOverrides;
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use chrono::Datelike;
    use std::env;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static TEST_DIR_LOCK: Mutex<()> = Mutex::new(());

    struct DirGuard {
        previous: PathBuf,
    }

    impl DirGuard {
        fn new(target: &std::path::Path) -> Self {
            let previous = env::current_dir().expect("current dir");
            env::set_current_dir(target).expect("set current dir");
            Self { previous }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.previous);
        }
    }

    fn make_issue(id: &str, title: &str) -> Issue {
        let now = Utc::now();
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: now,
            updated_at: now,
            content_hash: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_by: None,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
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
        }
    }

    #[test]
    fn test_parse_defer_time_rfc3339() {
        let result = parse_defer_time("2025-01-15T12:00:00Z").unwrap();
        assert_eq!(result.year(), 2025);
        assert_eq!(result.month(), 1);
        assert_eq!(result.day(), 15);
    }

    #[test]
    fn test_parse_defer_time_simple_date() {
        let result = parse_defer_time("2025-06-20").unwrap();
        assert_eq!(result.year(), 2025);
        assert_eq!(result.month(), 6);
        assert_eq!(result.day(), 20);
    }

    #[test]
    fn test_parse_defer_time_relative_hours() {
        let before = Utc::now();
        let result = parse_defer_time("+2h").unwrap();
        let after = Utc::now();

        // Result should be about 2 hours from now
        assert!(result > before + Duration::hours(1));
        assert!(result < after + Duration::hours(3));
    }

    #[test]
    fn test_parse_defer_time_relative_days() {
        let before = Utc::now();
        let result = parse_defer_time("+1d").unwrap();
        let after = Utc::now();

        // Result should be about 1 day from now
        assert!(result > before + Duration::hours(23));
        assert!(result < after + Duration::hours(25));
    }

    #[test]
    fn test_parse_defer_time_relative_weeks() {
        let before = Utc::now();
        let result = parse_defer_time("+1w").unwrap();
        let after = Utc::now();

        // Result should be about 1 week from now
        assert!(result > before + Duration::days(6));
        assert!(result < after + Duration::days(8));
    }

    #[test]
    fn test_parse_defer_time_tomorrow() {
        let result = parse_defer_time("tomorrow").unwrap();
        let expected_date = Local::now().date_naive() + Duration::days(1);

        // Check it's tomorrow (in UTC, might differ by a day due to timezone)
        let result_local = result.with_timezone(&Local);
        assert_eq!(result_local.date_naive(), expected_date);
    }

    #[test]
    fn test_parse_defer_time_next_week() {
        let result = parse_defer_time("next-week").unwrap();
        let expected_date = Local::now().date_naive() + Duration::weeks(1);

        let result_local = result.with_timezone(&Local);
        assert_eq!(result_local.date_naive(), expected_date);
    }

    #[test]
    fn test_parse_defer_time_invalid() {
        let result = parse_defer_time("invalid-time");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_defer_time_minutes() {
        let before = Utc::now();
        let result = parse_defer_time("+30m").unwrap();
        let after = Utc::now();

        // Result should be about 30 minutes from now
        assert!(result > before + Duration::minutes(29));
        assert!(result < after + Duration::minutes(31));
    }

    #[test]
    fn execute_defer_sets_status_and_until() {
        let _lock = TEST_DIR_LOCK.lock().expect("dir lock");
        let temp = TempDir::new().expect("tempdir");
        commands::init::execute(None, false, Some(temp.path())).expect("init");

        let beads_dir = temp.path().join(".beads");
        let mut storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("storage");
        let issue = make_issue("bd-defer-1", "Defer me");
        storage.create_issue(&issue, "tester").expect("create");

        let _guard = DirGuard::new(temp.path());
        let args = DeferArgs {
            ids: vec!["bd-defer-1".to_string()],
            until: Some("+1d".to_string()),
            robot: true,
        };
        execute_defer(&args, true, &CliOverrides::default()).expect("defer");

        let updated = storage.get_issue("bd-defer-1").expect("get").unwrap();
        assert_eq!(updated.status, Status::Deferred);
        assert!(updated.defer_until.is_some());
    }

    #[test]
    fn execute_defer_without_until_sets_indefinite() {
        let _lock = TEST_DIR_LOCK.lock().expect("dir lock");
        let temp = TempDir::new().expect("tempdir");
        commands::init::execute(None, false, Some(temp.path())).expect("init");

        let beads_dir = temp.path().join(".beads");
        let mut storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("storage");
        let issue = make_issue("bd-defer-2", "Defer me later");
        storage.create_issue(&issue, "tester").expect("create");

        let _guard = DirGuard::new(temp.path());
        let args = DeferArgs {
            ids: vec!["bd-defer-2".to_string()],
            until: None,
            robot: true,
        };
        execute_defer(&args, true, &CliOverrides::default()).expect("defer");

        let updated = storage.get_issue("bd-defer-2").expect("get").unwrap();
        assert_eq!(updated.status, Status::Deferred);
        assert!(updated.defer_until.is_none());
    }

    #[test]
    fn execute_undefer_clears_defer_until() {
        let _lock = TEST_DIR_LOCK.lock().expect("dir lock");
        let temp = TempDir::new().expect("tempdir");
        commands::init::execute(None, false, Some(temp.path())).expect("init");

        let beads_dir = temp.path().join(".beads");
        let mut storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("storage");
        let issue = make_issue("bd-defer-3", "Undefer me");
        storage.create_issue(&issue, "tester").expect("create");

        let _guard = DirGuard::new(temp.path());
        let defer_args = DeferArgs {
            ids: vec!["bd-defer-3".to_string()],
            until: Some("+1d".to_string()),
            robot: true,
        };
        execute_defer(&defer_args, true, &CliOverrides::default()).expect("defer");

        let undefer_args = UndeferArgs {
            ids: vec!["bd-defer-3".to_string()],
            robot: true,
        };
        execute_undefer(&undefer_args, true, &CliOverrides::default()).expect("undefer");

        let updated = storage.get_issue("bd-defer-3").expect("get").unwrap();
        assert_eq!(updated.status, Status::Open);
        assert!(updated.defer_until.is_none());
    }
}
