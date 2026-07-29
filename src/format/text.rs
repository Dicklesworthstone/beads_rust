//! Text formatting functions for `beads_rust`.
//!
//! Provides plain text (non-ANSI) formatting for terminal output:
//! - Status icons (○ ◐ ● ❄ ✓ ✗ 📌)
//! - Priority labels (P0-P4)
//! - Type badges ([bug], [feature], etc.)
//! - Issue line formatting

use crate::model::{Issue, IssueType, Priority, Status};
use crate::util::time::format_age_compact;
use chrono::Utc;
use crossterm::style::Stylize;
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
    pub wrap: bool,
}

impl TextFormatOptions {
    #[must_use]
    pub const fn plain() -> Self {
        Self {
            use_color: false,
            max_width: None,
            wrap: false,
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
        Status::Closed | Status::Tombstone => label.grey().to_string(),
        Status::Pinned => label.magenta().bold().to_string(),
        Status::Custom(_) => label.to_string(),
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
        Status::Closed | Status::Tombstone => icon.grey().to_string(),
        Status::Pinned => icon.magenta().bold().to_string(),
        Status::Custom(_) => icon.to_string(),
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
        3 | 4 => label.grey().to_string(),
        _ => label,
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
        IssueType::Task | IssueType::Custom(_) => label.to_string(),
        IssueType::Epic => label.magenta().bold().to_string(),
        IssueType::Docs | IssueType::Question => label.blue().to_string(),
        IssueType::Chore => label.grey().to_string(),
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

/// Fixed padded width of the age field in `format_issue_line_with`,
/// so the ` - title` column lines up across rows regardless of how
/// long any individual row's age string is. Sized to comfortably fit
/// the common dual form `NNw/NNw` (7 chars) with a little slack.
const AGE_FIELD_WIDTH: usize = 8;

/// Compute the compact "created/updated" age field for an issue —
/// e.g. `5d/2h` (created 5 days ago, updated 2 hours ago).
///
/// When the created and updated ages render to the *same* compact
/// string (the issue has never been meaningfully updated since
/// creation, at the resolution of the compact units), only that one
/// age is shown instead of a redundant `5d/5d`.
///
/// Shared with the rich `IssueTable`'s combined Age column so the
/// two views present ages identically.
#[must_use]
pub fn format_issue_age_field(issue: &Issue) -> String {
    let now = Utc::now();
    let created = format_age_compact((now - issue.created_at).num_seconds().max(0));
    let updated = format_age_compact((now - issue.updated_at).num_seconds().max(0));
    if created == updated {
        created
    } else {
        format!("{created}/{updated}")
    }
}

/// Format a single-line issue summary with options.
///
/// Format: `{icon} {id} [● {priority}] [{type}] {age} - {title}`
/// where `{age}` is the compact created/updated age field (see
/// [`format_issue_age_field`]), left-padded to a fixed width so
/// titles line up across rows.
#[must_use]
pub fn format_issue_line_with(issue: &Issue, options: TextFormatOptions) -> String {
    let status_icon_plain = format_status_icon(&issue.status);
    // Account for the bullet in priority badge: [● P2]
    let priority_badge_plain = format!("[● {}]", format_priority(&issue.priority));
    let type_badge_plain = format_type_badge(&issue.issue_type);
    let age_plain = format_issue_age_field(issue);
    let age_padded = format!("{age_plain:<AGE_FIELD_WIDTH$}");

    // Add 3 for " - " separator between age field and title
    let prefix_len = visible_len(status_icon_plain)
        + 1
        + visible_len(&issue.id)
        + 1
        + visible_len(&priority_badge_plain)
        + 1
        + visible_len(&type_badge_plain)
        + 1
        + visible_len(&age_padded)
        + 3; // " - " separator

    let title = if options.wrap {
        issue.title.clone()
    } else {
        options.max_width.map_or_else(
            || issue.title.clone(),
            |width| truncate_title(&issue.title, width.saturating_sub(prefix_len)),
        )
    };

    let status_icon = format_status_icon_colored(&issue.status, options.use_color);
    let priority_badge = format_priority_badge(&issue.priority, options.use_color);
    let type_badge = format_type_badge_colored(&issue.issue_type, options.use_color);
    let age = if options.use_color {
        age_padded.grey().to_string()
    } else {
        age_padded
    };

    format!(
        "{status_icon} {} {priority_badge} {type_badge} {age} - {title}",
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
            source_repo: None,
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
        // Fixed offsets (rather than `Utc::now()` for both fields)
        // avoid flakiness: a same-instant pair could round to "0s" or
        // "1s" depending on scheduling jitter between the two
        // `Utc::now()` calls in `make_test_issue`.
        let mut issue = make_test_issue();
        issue.created_at = Utc::now() - chrono::Duration::days(5);
        issue.updated_at = issue.created_at;
        let line = format_issue_line(&issue);
        // Format: {icon} {id} [● {priority}] [{type}] {age (padded)} - {title}
        // created == updated (same rendered unit) -> single "5d" age, not "5d/5d".
        assert_eq!(line, "○ bd-test [● P2] [task] 5d       - Test title");
    }

    #[test]
    fn test_format_issue_age_field_dedupes_equal_units() {
        let mut issue = make_test_issue();
        issue.created_at = Utc::now() - chrono::Duration::days(5);
        issue.updated_at = issue.created_at;
        assert_eq!(format_issue_age_field(&issue), "5d");
    }

    #[test]
    fn test_format_issue_age_field_shows_both_when_units_differ() {
        let mut issue = make_test_issue();
        issue.created_at = Utc::now() - chrono::Duration::days(5);
        issue.updated_at = Utc::now() - chrono::Duration::hours(2);
        assert_eq!(format_issue_age_field(&issue), "5d/2h");
    }

    #[test]
    fn test_format_issue_line_age_is_padded_for_alignment() {
        let mut short = make_test_issue();
        short.created_at = Utc::now() - chrono::Duration::minutes(5);
        short.updated_at = short.created_at;
        short.title = "Short-age title".to_string();

        let mut long = make_test_issue();
        long.created_at = Utc::now() - chrono::Duration::weeks(3);
        long.updated_at = Utc::now() - chrono::Duration::hours(6);
        long.title = "Long-age title".to_string();

        let short_line = format_issue_line(&short);
        let long_line = format_issue_line(&long);
        // Both lines' " - " title separator should land at the same
        // column, since the age field is padded to a fixed width.
        let short_dash = short_line.find(" - ").expect("dash in short line");
        let long_dash = long_line.find(" - ").expect("dash in long line");
        assert_eq!(short_dash, long_dash);
    }

    #[test]
    fn test_format_issue_line_age_colored_when_color_on() {
        let mut issue = make_test_issue();
        issue.created_at = Utc::now() - chrono::Duration::days(5);
        issue.updated_at = issue.created_at;
        let options = TextFormatOptions {
            use_color: true,
            max_width: None,
            wrap: false,
        };
        let line = format_issue_line_with(&issue, options);
        // Grey ANSI SGR code wraps the age text when color is on.
        assert!(line.contains("\x1b["));
    }

    #[test]
    fn test_format_issue_line_age_plain_when_color_off() {
        let mut issue = make_test_issue();
        issue.created_at = Utc::now() - chrono::Duration::days(5);
        issue.updated_at = issue.created_at;
        let line = format_issue_line(&issue);
        assert!(!line.contains("\x1b["));
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
        // max_width must clear the (larger, now age-inclusive) prefix
        // with some budget left over for the title, or truncate_title
        // has 0 columns to work with and can't add "...".
        let options = TextFormatOptions {
            use_color: false,
            max_width: Some(50),
            wrap: false,
        };
        let line = format_issue_line_with(&issue, options);
        assert!(line.contains("..."));
    }

    #[test]
    fn test_format_issue_line_with_wrap() {
        let mut issue = make_test_issue();
        issue.title = "A very long issue title".to_string();
        let options = TextFormatOptions {
            use_color: false,
            max_width: Some(20),
            wrap: true,
        };
        let line = format_issue_line_with(&issue, options);
        assert!(!line.contains("..."));
        assert!(line.contains("A very long issue title"));
    }
}
