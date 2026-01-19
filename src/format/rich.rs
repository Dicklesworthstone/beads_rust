//! Rich terminal output components using rich_rust.
//!
//! This module provides enhanced visual output for TTY terminals
//! while preserving backward compatibility with existing format functions.
//!
//! # Components
//!
//! - [`RichIssueTable`]: Display issues in a formatted table
//! - [`RichIssuePanel`]: Display single issue details in a panel
//! - [`RichDependencyTree`]: Display dependency graphs as trees
//! - [`format_count_badges`]: Display status count badges
//! - [`build_completion_bar`]: Display progress indicator
//!
//! # Integration
//!
//! These components work alongside existing text.rs functions:
//!
//! ```ignore
//! use crate::format::{OutputContext, OutputMode, Theme};
//! use crate::format::rich::RichIssueTable;
//!
//! let ctx = OutputContext::detect();
//! match ctx.mode() {
//!     OutputMode::Rich => {
//!         let table = RichIssueTable::new(&issues, &Theme::new());
//!         console.print_renderable(&table.build_table());
//!     }
//!     OutputMode::Plain => {
//!         // Use existing text.rs functions
//!         for issue in &issues {
//!             println!("{}", format_issue_line(issue));
//!         }
//!     }
//!     _ => { /* JSON/Quiet handled elsewhere */ }
//! }
//! ```

use crate::format::text::{format_priority, format_status_icon, truncate_title};
use crate::format::theme::Theme;
use crate::model::{Issue, Status};
use rich_rust::prelude::*;

/// Rich table display for a list of issues.
///
/// Displays issues with columns for status, ID, priority, type, and title.
pub struct RichIssueTable<'a> {
    issues: &'a [Issue],
    theme: &'a Theme,
    show_type: bool,
    max_title_width: Option<usize>,
}

impl<'a> RichIssueTable<'a> {
    /// Create a new issue table.
    #[must_use]
    pub fn new(issues: &'a [Issue], theme: &'a Theme) -> Self {
        Self {
            issues,
            theme,
            show_type: true,
            max_title_width: None,
        }
    }

    /// Set whether to show the issue type column.
    #[must_use]
    pub const fn show_type(mut self, show: bool) -> Self {
        self.show_type = show;
        self
    }

    /// Set maximum title width for truncation.
    #[must_use]
    pub const fn max_title_width(mut self, width: usize) -> Self {
        self.max_title_width = Some(width);
        self
    }

    /// Build a rich_rust Table from the issues.
    #[must_use]
    pub fn build_table(&self) -> Table {
        let mut table = Table::new()
            .with_column(Column::new("").width(2)) // Status icon
            .with_column(Column::new("ID"))
            .with_column(Column::new("Pri").justify(JustifyMethod::Center));

        if self.show_type {
            table = table.with_column(Column::new("Type"));
        }

        table = table.with_column(Column::new("Title"));

        for issue in self.issues {
            let status_icon = format_status_icon(&issue.status);
            let priority = format_priority(&issue.priority);
            let title = self.max_title_width.map_or_else(
                || issue.title.clone(),
                |w| truncate_title(&issue.title, w),
            );

            let mut cells = Vec::new();

            // Status icon with color
            let status_style = self.theme.status_style(&issue.status);
            cells.push(Cell::new(status_icon).style(status_style.clone()));

            // Issue ID
            cells.push(Cell::new(&*issue.id).style(self.theme.issue_id.clone()));

            // Priority with color
            let priority_style = self.theme.priority_style(&issue.priority);
            cells.push(Cell::new(priority).style(priority_style.clone()));

            // Type (if showing)
            if self.show_type {
                let type_str = issue.issue_type.as_str();
                let type_style = self.theme.type_style(&issue.issue_type);
                cells.push(Cell::new(type_str).style(type_style.clone()));
            }

            // Title
            cells.push(Cell::new(title));

            table = table.with_row(Row::new(cells));
        }

        table
    }
}

/// Rich panel display for a single issue with full details.
pub struct RichIssuePanel<'a> {
    issue: &'a Issue,
    theme: &'a Theme,
    show_description: bool,
}

impl<'a> RichIssuePanel<'a> {
    /// Create a new issue panel.
    #[must_use]
    pub const fn new(issue: &'a Issue, theme: &'a Theme) -> Self {
        Self {
            issue,
            theme,
            show_description: true,
        }
    }

    /// Set whether to show the description.
    #[must_use]
    pub const fn show_description(mut self, show: bool) -> Self {
        self.show_description = show;
        self
    }

    /// Build a rich_rust Panel from the issue.
    #[must_use]
    pub fn build_panel(&self) -> Panel<'static> {
        let mut content = String::new();

        // Header line: status icon + title
        let status_icon = format_status_icon(&self.issue.status);
        content.push_str(&format!("{} {}\n", status_icon, self.issue.title));

        // Metadata line
        let priority = format_priority(&self.issue.priority);
        let type_str = self.issue.issue_type.as_str();
        let status_str = self.issue.status.as_str();
        content.push_str(&format!(
            "[{priority}] [{type_str}] {status_str}\n"
        ));

        // Description if present and enabled
        if self.show_description {
            if let Some(desc) = &self.issue.description {
                content.push('\n');
                content.push_str(desc);
            }
        }

        let status_style = self.theme.status_style(&self.issue.status);

        // Build content lines for Panel
        let content_lines: Vec<Vec<Segment<'static>>> = content
            .lines()
            .map(|line| vec![Segment::new(line.to_owned(), None)])
            .collect();

        Panel::new(content_lines)
            .title(self.issue.id.clone())
            .border_style(status_style.clone())
    }
}

/// Rich tree display for dependency relationships.
pub struct RichDependencyTree<'a> {
    root_issue: &'a Issue,
    dependencies: &'a [(String, String)], // (from_id, to_id)
    issues_by_id: &'a std::collections::HashMap<String, &'a Issue>,
    #[allow(dead_code)] // Reserved for future styling
    theme: &'a Theme,
}

impl<'a> RichDependencyTree<'a> {
    /// Create a new dependency tree.
    #[must_use]
    pub fn new(
        root_issue: &'a Issue,
        dependencies: &'a [(String, String)],
        issues_by_id: &'a std::collections::HashMap<String, &'a Issue>,
        theme: &'a Theme,
    ) -> Self {
        Self {
            root_issue,
            dependencies,
            issues_by_id,
            theme,
        }
    }

    /// Build a rich_rust Tree from the dependencies.
    #[must_use]
    pub fn build_tree(&self) -> Tree {
        let root_label = format!(
            "{} {} - {}",
            format_status_icon(&self.root_issue.status),
            self.root_issue.id,
            truncate_title(&self.root_issue.title, 40)
        );

        let mut root = TreeNode::new(root_label);

        // Find direct dependencies of root
        for (from_id, to_id) in self.dependencies {
            if from_id == &self.root_issue.id {
                if let Some(dep_issue) = self.issues_by_id.get(to_id) {
                    let dep_label = format!(
                        "{} {} - {}",
                        format_status_icon(&dep_issue.status),
                        dep_issue.id,
                        truncate_title(&dep_issue.title, 35)
                    );
                    root = root.child(TreeNode::new(dep_label));
                } else {
                    let dep_label = format!("? {} - (unknown)", to_id);
                    root = root.child(TreeNode::new(dep_label));
                }
            }
        }

        Tree::new(root)
    }
}

/// Format a status badge with rich styling.
#[must_use]
pub fn format_status_badge(status: &Status, theme: &Theme) -> Text {
    let icon = format_status_icon(status);
    let label = status.as_str().to_uppercase();
    let style = theme.status_style(status);

    let mut text = Text::new("");
    text.append_styled(&format!("{icon} {label}"), style.clone());
    text
}

/// Format a count badge (e.g., "3 open, 2 blocked").
#[must_use]
pub fn format_count_badges(
    open: usize,
    in_progress: usize,
    blocked: usize,
    closed: usize,
    theme: &Theme,
) -> Text {
    let mut text = Text::new("");
    let mut needs_space = false;

    if open > 0 {
        text.append_styled(&format!("{open} open"), theme.status_open.clone());
        needs_space = true;
    }
    if in_progress > 0 {
        if needs_space {
            text.append(" ");
        }
        text.append_styled(
            &format!("{in_progress} in progress"),
            theme.status_in_progress.clone(),
        );
        needs_space = true;
    }
    if blocked > 0 {
        if needs_space {
            text.append(" ");
        }
        text.append_styled(&format!("{blocked} blocked"), theme.status_blocked.clone());
        needs_space = true;
    }
    if closed > 0 {
        if needs_space {
            text.append(" ");
        }
        text.append_styled(&format!("{closed} closed"), theme.status_closed.clone());
    }

    text
}

/// Build a rich progress indicator for issue completion.
#[must_use]
pub fn build_completion_bar(completed: usize, total: usize, _theme: &Theme) -> ProgressBar {
    let progress = if total > 0 {
        completed as f64 / total as f64
    } else {
        0.0
    };

    let mut bar = ProgressBar::new()
        .width(20)
        .bar_style(BarStyle::Block);
    bar.set_progress(progress);
    bar
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IssueType, Priority};
    use chrono::Utc;

    fn make_test_issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            description: Some("Test description".to_string()),
            issue_type: IssueType::Task,
            status: Status::Open,
            priority: Priority::MEDIUM,
            assignee: None,
            labels: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            content_hash: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
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
            dependencies: vec![],
            comments: vec![],
        }
    }

    #[test]
    fn test_rich_issue_table() {
        let issues = vec![
            make_test_issue("test-1", "First issue"),
            make_test_issue("test-2", "Second issue"),
        ];
        let theme = Theme::new();
        let table = RichIssueTable::new(&issues, &theme);
        let _ = table.build_table();
    }

    #[test]
    fn test_rich_issue_panel() {
        let issue = make_test_issue("test-1", "Test issue");
        let theme = Theme::new();
        let panel = RichIssuePanel::new(&issue, &theme);
        let _ = panel.build_panel();
    }

    #[test]
    fn test_format_status_badge() {
        let theme = Theme::new();
        let _ = format_status_badge(&Status::Open, &theme);
        let _ = format_status_badge(&Status::Blocked, &theme);
    }

    #[test]
    fn test_format_count_badges() {
        let theme = Theme::new();
        let _ = format_count_badges(5, 2, 1, 3, &theme);
    }

    #[test]
    fn test_build_completion_bar() {
        let theme = Theme::new();
        let _ = build_completion_bar(7, 10, &theme);
    }

    #[test]
    fn test_table_without_type_column() {
        let issues = vec![make_test_issue("test-1", "Issue")];
        let theme = Theme::new();
        let table = RichIssueTable::new(&issues, &theme).show_type(false);
        let _ = table.build_table();
    }

    #[test]
    fn test_table_with_title_truncation() {
        let issues = vec![make_test_issue("test-1", "A very long title that should be truncated")];
        let theme = Theme::new();
        let table = RichIssueTable::new(&issues, &theme).max_title_width(20);
        let _ = table.build_table();
    }
}
