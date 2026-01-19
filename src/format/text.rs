//! Text formatting functions for `beads_rust`.
//!
//! Provides plain text (non-ANSI) formatting for terminal output:
//! - Status icons (○ ◐ ● ❄ ✓ ✗ 📌)
//! - Priority labels (P0-P4)
//! - Type badges ([bug], [feature], etc.)
//! - Issue line formatting

use crate::model::{Issue, IssueType, Priority, Status};
use colored::Colorize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Status icon characters.
pub mod icons {
    /// Open issue - available to work (hollow circle).
    pub const OPEN: &str = "○";
    /// In progress - active work (half-filled).
    pub const IN_PROGRESS: &str = "◐";
    /// Blocked - needs attention (filled circle).
    pub const BLOCKED: &str = "●";
    /// Deferred - scheduled for later (snowflake).
    pub const DEFERRED: &str = "❄";
    /// Closed - completed (checkmark).
    pub const CLOSED: &str = "✓";
    /// Tombstone - soft deleted (X mark).
    pub const TOMBSTONE: &str = "✗";
    /// Pinned - elevated priority (pushpin).
    pub const PINNED: &str = "📌";
    /// Unknown status.
    pub const UNKNOWN: &str = "?";
}

/// Formatting options for text output.
#[derive(Debug, Clone, Copy)]
pub struct TextFormatOptions {
    pub use_color: bool,
    pub max_width: Option<usize>,
}

impl TextFormatOptions {
    #[must_use]
    pub const fn plain() -> Self {
        Self {
            use_color: false,
            max_width: None,
        }
    }
}

/// Return the icon character for a status.
#[must_use]
pub const fn format_status_icon(status: &Status) -> &'static str {
    match status {
        Status::Open => icons::OPEN,
        Status::InProgress => icons::IN_PROGRESS,
        Status::Blocked => icons::BLOCKED,
        Status::Deferred => icons::DEFERRED,
        Status::Closed => icons::CLOSED,
        Status::Tombstone => icons::TOMBSTONE,
        Status::Pinned => icons::PINNED,
        Status::Custom(_) => icons::UNKNOWN,
    }
}

/// Format priority as "P0", "P1", etc.
#[must_use]
pub fn format_priority(priority: &Priority) -> String {
    format!("P{}", priority.0)
}

/// Format status label with optional color.
#[must_use]
pub fn format_status_label(status: &Status, use_color: bool) -> String {
    let label = status.as_str();
    if !use_color {
        return label.to_string();
    }

    match status {
        Status::Open => label.green().to_string(),
        Status::InProgress => label.yellow().to_string(),
        Status::Blocked => label.red().to_string(),
        Status::Deferred => label.blue().to_string(),
        Status::Closed | Status::Tombstone => label.bright_black().to_string(),
        Status::Pinned => label.magenta().bold().to_string(),
        Status::Custom(_) => label.normal().to_string(),
    }
}

/// Format status icon with optional color.
#[must_use]
pub fn format_status_icon_colored(status: &Status, use_color: bool) -> String {
    let icon = format_status_icon(status);
    if !use_color {
        return icon.to_string();
    }

    match status {
        Status::Open => icon.green().to_string(),
        Status::InProgress => icon.yellow().to_string(),
        Status::Blocked => icon.red().to_string(),
        Status::Deferred => icon.blue().to_string(),
        Status::Closed | Status::Tombstone => icon.bright_black().to_string(),
        Status::Pinned => icon.magenta().bold().to_string(),
        Status::Custom(_) => icon.normal().to_string(),
    }
}

/// Format priority label with optional color.
#[must_use]
pub fn format_priority_label(priority: &Priority, use_color: bool) -> String {
    let label = format_priority(priority);
    if !use_color {
        return label;
    }

    match priority.0 {
        0 => label.red().bold().to_string(),
        1 => label.red().to_string(),
        2 => label.yellow().to_string(),
        3 | 4 => label.bright_black().to_string(),
        _ => label.normal().to_string(),
    }
}

/// Format priority badge with optional color.
///
/// Matches bd format: `[● P2]` (bullet before priority number).
#[must_use]
pub fn format_priority_badge(priority: &Priority, use_color: bool) -> String {
    format!("[● {}]", format_priority_label(priority, use_color))
}

/// Format issue type as a bracketed badge.
#[must_use]
pub fn format_type_badge(issue_type: &IssueType) -> String {
    format!("[{}]", issue_type.as_str())
}

/// Format issue type badge with optional color.
#[must_use]
pub fn format_type_badge_colored(issue_type: &IssueType, use_color: bool) -> String {
    let label = issue_type.as_str();
    if !use_color {
        return format!("[{label}]");
    }

    let colored = match issue_type {
        IssueType::Bug => label.red().to_string(),
        IssueType::Feature => label.cyan().to_string(),
        IssueType::Task | IssueType::Custom(_) => label.normal().to_string(),
        IssueType::Epic => label.magenta().bold().to_string(),
        IssueType::Docs | IssueType::Question => label.blue().to_string(),
        IssueType::Chore => label.bright_black().to_string(),
    };

    format!("[{colored}]")
}

/// Determine terminal width from environment (falls back to 80).
///
/// Checks in order:
/// 1. `COLUMNS` environment variable
/// 2. Terminal size via crossterm
/// 3. Falls back to 80
#[must_use]
pub fn terminal_width() -> usize {
    // Try COLUMNS first
    if let Ok(columns) = std::env::var("COLUMNS") {
        if let Ok(value) = columns.trim().parse::<usize>() {
            if value > 0 {
                return value;
            }
        }
    }

    // Try crossterm for actual terminal size
    if let Ok((cols, _)) = crossterm::terminal::size() {
        if cols > 0 {
            return cols as usize;
        }
    }

    80
}

/// Truncate a title to fit within `max_len` visible columns.
///
/// Handles wide characters (emojis, CJK) correctly using `unicode-width`.
#[must_use]
pub fn truncate_title(title: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }

    let width = UnicodeWidthStr::width(title);
    if width <= max_len {
        return title.to_string();
    }

    if max_len <= 3 {
        let mut w = 0;
        let mut s = String::new();
        for c in title.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw > max_len {
                break;
            }
            w += cw;
            s.push(c);
        }
        return s;
    }

    let target_len = max_len - 3;
    let mut w = 0;
    let mut s = String::new();
    for c in title.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > target_len {
            break;
        }
        w += cw;
        s.push(c);
    }
    s.push_str("...");
    s
}

fn visible_len(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Format a single-line issue summary with options.
///
/// Format: `{icon} {id} [● {priority}] [{type}] - {title}`
/// (matches bd text output format)
#[must_use]
pub fn format_issue_line_with(issue: &Issue, options: TextFormatOptions) -> String {
    let status_icon_plain = format_status_icon(&issue.status);
    // Account for the bullet in priority badge: [● P2]
    let priority_badge_plain = format!("[● {}]", format_priority(&issue.priority));
    let type_badge_plain = format_type_badge(&issue.issue_type);

    // Add 3 for " - " separator between type badge and title
    let prefix_len = visible_len(status_icon_plain)
        + 1
        + visible_len(&issue.id)
        + 1
        + visible_len(&priority_badge_plain)
        + 1
        + visible_len(&type_badge_plain)
        + 3; // " - " separator

    let title = options.max_width.map_or_else(
        || issue.title.clone(),
        |width| truncate_title(&issue.title, width.saturating_sub(prefix_len)),
    );

    let status_icon = format_status_icon_colored(&issue.status, options.use_color);
    let priority_badge = format_priority_badge(&issue.priority, options.use_color);
    let type_badge = format_type_badge_colored(&issue.issue_type, options.use_color);

    format!(
        "{status_icon} {} {priority_badge} {type_badge} - {title}",
        issue.id
    )
}

/// Format a single-line issue summary.
///
/// Format: `{icon} {id} [{priority}] [{type}] {title}`
#[must_use]
pub fn format_issue_line(issue: &Issue) -> String {
    format_issue_line_with(issue, TextFormatOptions::plain())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_test_issue() -> Issue {
        Issue {
            id: "bd-test".to_string(),
            content_hash: None,
            title: "Test title".to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc::now(),
            created_by: None,
            updated_at: Utc::now(),
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
    fn test_status_icons() {
        assert_eq!(format_status_icon(&Status::Open), "○");
        assert_eq!(format_status_icon(&Status::InProgress), "◐");
        assert_eq!(format_status_icon(&Status::Blocked), "●");
        assert_eq!(format_status_icon(&Status::Deferred), "❄");
        assert_eq!(format_status_icon(&Status::Closed), "✓");
        assert_eq!(format_status_icon(&Status::Tombstone), "✗");
        assert_eq!(format_status_icon(&Status::Pinned), "📌");
        assert_eq!(
            format_status_icon(&Status::Custom("custom".to_string())),
            "?"
        );
    }

    #[test]
    fn test_format_priority() {
        assert_eq!(format_priority(&Priority::CRITICAL), "P0");
        assert_eq!(format_priority(&Priority::HIGH), "P1");
        assert_eq!(format_priority(&Priority::MEDIUM), "P2");
        assert_eq!(format_priority(&Priority::LOW), "P3");
        assert_eq!(format_priority(&Priority::BACKLOG), "P4");
    }

    #[test]
    fn test_format_type_badge() {
        assert_eq!(format_type_badge(&IssueType::Task), "[task]");
        assert_eq!(format_type_badge(&IssueType::Bug), "[bug]");
        assert_eq!(format_type_badge(&IssueType::Feature), "[feature]");
        assert_eq!(format_type_badge(&IssueType::Epic), "[epic]");
        assert_eq!(format_type_badge(&IssueType::Chore), "[chore]");
        assert_eq!(format_type_badge(&IssueType::Docs), "[docs]");
        assert_eq!(format_type_badge(&IssueType::Question), "[question]");
        assert_eq!(
            format_type_badge(&IssueType::Custom("custom".to_string())),
            "[custom]"
        );
    }

    #[test]
    fn test_format_issue_line_open() {
        let issue = make_test_issue();
        let line = format_issue_line(&issue);
        // Format matches bd: {icon} {id} [● {priority}] [{type}] - {title}
        assert_eq!(line, "○ bd-test [● P2] [task] - Test title");
    }

    #[test]
    fn test_format_issue_line_in_progress() {
        let mut issue = make_test_issue();
        issue.status = Status::InProgress;
        let line = format_issue_line(&issue);
        assert!(line.starts_with("◐"));
    }

    #[test]
    fn test_format_issue_line_closed() {
        let mut issue = make_test_issue();
        issue.status = Status::Closed;
        let line = format_issue_line(&issue);
        assert!(line.starts_with("✓"));
    }

    #[test]
    fn test_format_issue_line_bug_high_priority() {
        let mut issue = make_test_issue();
        issue.issue_type = IssueType::Bug;
        issue.priority = Priority::HIGH;
        issue.title = "Critical bug".to_string();
        let line = format_issue_line(&issue);
        assert!(line.contains("[● P1]"));
        assert!(line.contains("[bug]"));
        assert!(line.contains("Critical bug"));
    }

    #[test]
    fn test_format_issue_line_epic() {
        let mut issue = make_test_issue();
        issue.issue_type = IssueType::Epic;
        issue.priority = Priority::CRITICAL;
        let line = format_issue_line(&issue);
        assert!(line.contains("[● P0]"));
        assert!(line.contains("[epic]"));
    }

    #[test]
    fn test_format_issue_line_blocked() {
        let mut issue = make_test_issue();
        issue.status = Status::Blocked;
        let line = format_issue_line(&issue);
        assert!(line.starts_with("●"));
    }

    #[test]
    fn test_format_issue_line_deferred() {
        let mut issue = make_test_issue();
        issue.status = Status::Deferred;
        let line = format_issue_line(&issue);
        assert!(line.starts_with("❄"));
    }

    #[test]
    fn test_truncate_title_adds_ellipsis() {
        let title = "This is a long title";
        let truncated = truncate_title(title, 10);
        assert_eq!(truncated, "This is...");
    }

    #[test]
    fn test_format_issue_line_with_truncation() {
        let mut issue = make_test_issue();
        issue.title = "A very long issue title".to_string();
        let options = TextFormatOptions {
            use_color: false,
            max_width: Some(30),
        };
        let line = format_issue_line_with(&issue, options);
        assert!(line.contains("..."));
    }
}
