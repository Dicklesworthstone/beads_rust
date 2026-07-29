//! Text formatting functions for `beads_rust`.
//!
//! Provides plain text (non-ANSI) formatting for terminal output:
//! - Status icons (○ ◐ ● ❄ ✓ ✗ 📌)
//! - Priority labels (P0-P4)
//! - Type badges ([bug], [feature], etc.)
//! - Issue line formatting

use crate::model::{Issue, IssueType, Priority, Status};
use crossterm::style::Stylize;
use std::borrow::Cow;
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

/// Escape terminal control characters before rendering untrusted text.
///
/// This preserves ordinary printable text while making ANSI/OSC/C0/C1 controls
/// visible as Rust-style escapes such as `\u{1b}`. Use the inline variant for
/// titles, labels, authors, and other single-line fields.
#[must_use]
pub fn sanitize_terminal_inline(text: &str) -> Cow<'_, str> {
    sanitize_terminal_controls(text, false)
}

/// Escape terminal control characters before rendering trusted line structure.
///
/// This preserves `\n` and `\t` for multi-line descriptions and comments, but
/// still escapes carriage returns, backspaces, ESC, BEL, DEL, and C1 controls.
#[must_use]
pub fn sanitize_terminal_text(text: &str) -> Cow<'_, str> {
    sanitize_terminal_controls(text, true)
}

fn sanitize_terminal_controls(text: &str, preserve_layout_controls: bool) -> Cow<'_, str> {
    let mut escaped = String::new();
    let mut changed = false;

    for (idx, ch) in text.char_indices() {
        let allowed_layout = preserve_layout_controls && matches!(ch, '\n' | '\t');
        if allowed_layout || !ch.is_control() {
            if changed {
                escaped.push(ch);
            }
            continue;
        }

        if !changed {
            escaped.reserve(text.len());
            escaped.push_str(&text[..idx]);
            changed = true;
        }

        for escaped_char in ch.escape_default() {
            escaped.push(escaped_char);
        }
    }

    if changed {
        Cow::Owned(escaped)
    } else {
        Cow::Borrowed(text)
    }
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
        Status::Deferred | Status::Draft => icons::DEFERRED,
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
    let label = sanitize_terminal_inline(status.as_str());
    let label = label.as_ref();
    if !use_color {
        return label.to_string();
    }

    match status {
        Status::Open => label.green().to_string(),
        Status::InProgress => label.yellow().to_string(),
        Status::Blocked => label.red().to_string(),
        Status::Deferred | Status::Draft => label.blue().to_string(),
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
        Status::Deferred | Status::Draft => icon.blue().to_string(),
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
    format!("[{}]", format_type_label(issue_type))
}

/// Format issue type text for display.
#[must_use]
pub fn format_type_label(issue_type: &IssueType) -> String {
    sanitize_terminal_inline(issue_type.as_str()).into_owned()
}

/// Format issue type badge with optional color.
#[must_use]
pub fn format_type_badge_colored(issue_type: &IssueType, use_color: bool) -> String {
    let label = format_type_label(issue_type);
    if !use_color {
        return format!("[{label}]");
    }

    let colored = match issue_type {
        IssueType::Bug => label.as_str().red().to_string(),
        IssueType::Feature => label.as_str().cyan().to_string(),
        IssueType::Task | IssueType::Custom(_) => label,
        IssueType::Epic => label.as_str().magenta().bold().to_string(),
        IssueType::Docs | IssueType::Question => label.as_str().blue().to_string(),
        IssueType::Chore => label.as_str().grey().to_string(),
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
    if let Ok(columns) = std::env::var("COLUMNS")
        && let Ok(value) = columns.trim().parse::<usize>()
        && value > 0
    {
        return value;
    }

    // Try crossterm for actual terminal size
    if let Ok((cols, _)) = crossterm::terminal::size()
        && cols > 0
    {
        return cols as usize;
    }

    80
}

/// Truncate a title to fit within `max_len` visible columns.
///
/// Handles wide characters (emojis, CJK) correctly using `unicode-width`.
#[must_use]
pub fn truncate_title(title: &str, max_len: usize) -> String {
    let title = sanitize_terminal_inline(title);
    let title = title.as_ref();

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
    let issue_id = sanitize_terminal_inline(&issue.id);

    // Add 3 for " - " separator between type badge and title
    let prefix_len = visible_len(status_icon_plain)
        + 1
        + visible_len(issue_id.as_ref())
        + 1
        + visible_len(&priority_badge_plain)
        + 1
        + visible_len(&type_badge_plain)
        + 3; // " - " separator

    let sanitized_title = sanitize_terminal_inline(&issue.title);
    let title = if options.wrap {
        sanitized_title.into_owned()
    } else if let Some(width) = options.max_width {
        truncate_title(&issue.title, width.saturating_sub(prefix_len))
    } else {
        sanitized_title.into_owned()
    };

    let status_icon = format_status_icon_colored(&issue.status, options.use_color);
    let priority_badge = format_priority_badge(&issue.priority, options.use_color);
    let type_badge = format_type_badge_colored(&issue.issue_type, options.use_color);

    format!(
        "{status_icon} {issue_id} {priority_badge} {type_badge} - {title}",
        issue_id = issue_id.as_ref()
    )
}

/// Format a single-line issue summary.
///
/// Format: `{icon} {id} [{priority}] [{type}] {title}`
#[must_use]
pub fn format_issue_line(issue: &Issue) -> String {
    format_issue_line_with(issue, TextFormatOptions::plain())
}

fn issue_detail_lines(issue: &Issue, include_extended: bool) -> Vec<String> {
    let labels = issue
        .labels
        .iter()
        .map(|label| sanitize_terminal_inline(label).into_owned())
        .collect::<Vec<_>>();
    let mut details = vec![
        format!("Status: {}", format_status_label(&issue.status, false)),
        format!("Priority: {}", format_priority(&issue.priority)),
        format!("Type: {}", format_type_label(&issue.issue_type)),
    ];

    if let Some(assignee) = issue.assignee.as_deref()
        && !assignee.is_empty()
    {
        details.push(format!("Assignee: {}", sanitize_terminal_inline(assignee)));
    }
    if let Some(owner) = issue.owner.as_deref()
        && !owner.is_empty()
    {
        details.push(format!("Owner: {}", sanitize_terminal_inline(owner)));
    }
    if !labels.is_empty() {
        details.push(format!("Labels: {}", labels.join(", ")));
    }
    if let Some(due_at) = issue.due_at {
        details.push(format!("Due: {}", due_at.format("%Y-%m-%d")));
    }
    if let Some(defer_until) = issue.defer_until {
        details.push(format!(
            "Deferred Until: {}",
            defer_until.format("%Y-%m-%d")
        ));
    }

    if include_extended {
        details.push(format!("Created: {}", issue.created_at.format("%Y-%m-%d")));
        details.push(format!("Updated: {}", issue.updated_at.format("%Y-%m-%d")));
    }

    details
}

/// Format an issue as a multi-line long text block.
#[must_use]
pub fn format_issue_long_with(issue: &Issue, options: TextFormatOptions) -> String {
    let mut lines = vec![format_issue_line_with(issue, options)];
    lines.extend(
        issue_detail_lines(issue, true)
            .into_iter()
            .map(|detail| format!("  {detail}")),
    );
    lines.join("\n")
}

/// Format an issue as a tree-style pretty text block.
#[must_use]
pub fn format_issue_pretty_with(
    issue: &Issue,
    options: TextFormatOptions,
    include_extended: bool,
) -> String {
    let mut lines = vec![format_issue_line_with(issue, options)];
    let details = issue_detail_lines(issue, include_extended);
    let detail_count = details.len();

    for (index, detail) in details.into_iter().enumerate() {
        let branch = if index + 1 == detail_count {
            "└──"
        } else {
            "├──"
        };
        lines.push(format!("{branch} {detail}"));
    }

    lines.join("\n")
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
    fn status_and_type_labels_escape_terminal_controls() {
        let status = format_status_label(&Status::Custom("bad\x1b[2J".to_string()), false);
        let issue_type = format_type_badge(&IssueType::Custom("kind\x07bell".to_string()));

        assert!(!status.chars().any(char::is_control));
        assert!(status.contains("\\u{1b}[2J"));
        assert!(!issue_type.chars().any(char::is_control));
        assert!(issue_type.contains("\\u{7}bell"));
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

    #[test]
    fn test_format_issue_long_with_includes_extended_fields() {
        let mut issue = make_test_issue();
        issue.assignee = Some("alice".to_string());
        issue.labels = vec!["core".to_string(), "frontend".to_string()];

        let output = format_issue_long_with(&issue, TextFormatOptions::plain());

        assert!(output.contains("Status: open"));
        assert!(output.contains("Priority: P2"));
        assert!(output.contains("Assignee: alice"));
        assert!(output.contains("Labels: core, frontend"));
        assert!(output.contains("Created: "));
        assert!(output.contains("Updated: "));
    }

    #[test]
    fn test_format_issue_long_with_sanitizes_custom_status_and_type() {
        let mut issue = make_test_issue();
        issue.id = "bd-test\x1b]52;c;bad\x07".to_string();
        issue.status = Status::Custom("state\x1b[31m".to_string());
        issue.issue_type = IssueType::Custom("kind\x07alert".to_string());

        let output = format_issue_long_with(&issue, TextFormatOptions::plain());

        assert!(!output.contains('\x1b'));
        assert!(!output.contains('\x07'));
        assert!(output.contains("bd-test\\u{1b}]52;c;bad\\u{7}"));
        assert!(output.contains("Status: state\\u{1b}[31m"));
        assert!(output.contains("Type: kind\\u{7}alert"));
    }

    #[test]
    fn test_format_issue_pretty_with_uses_tree_connectors() {
        let mut issue = make_test_issue();
        issue.assignee = Some("alice".to_string());

        let output = format_issue_pretty_with(&issue, TextFormatOptions::plain(), false);

        assert!(output.contains("├── Status: open"));
        assert!(output.contains("├── Priority: P2"));
        assert!(output.contains("└── Assignee: alice"));
        assert!(!output.contains("Created: "));
    }

    #[test]
    fn terminal_inline_escapes_control_sequences() {
        let sanitized = sanitize_terminal_inline("bad\x1b[2J\rreset\x08\nnext\x07\u{9b}");

        assert!(!sanitized.chars().any(char::is_control));
        assert!(sanitized.contains("\\u{1b}[2J"));
        assert!(sanitized.contains("\\r"));
        assert!(sanitized.contains("\\u{8}"));
        assert!(sanitized.contains("\\n"));
        assert!(sanitized.contains("\\u{7}"));
        assert!(sanitized.contains("\\u{9b}"));
    }

    #[test]
    fn terminal_text_preserves_layout_but_escapes_controls() {
        let sanitized = sanitize_terminal_text("line one\n\tline two\x1b]52;c;bad\x07\r");

        assert!(sanitized.contains("line one\n\tline two"));
        assert!(sanitized.contains("\\u{1b}]52"));
        assert!(sanitized.contains("\\u{7}"));
        assert!(sanitized.contains("\\r"));
        assert!(!sanitized.contains('\x1b'));
        assert!(!sanitized.contains('\x07'));
        assert!(!sanitized.contains('\r'));
    }
}
