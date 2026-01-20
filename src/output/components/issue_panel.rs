use crate::model::Issue;
use crate::output::{OutputContext, Theme};
use rich_rust::prelude::*;

/// Renders a single issue with full details in a styled panel.
pub struct IssuePanel<'a> {
    issue: &'a Issue,
    theme: &'a Theme,
    show_dependencies: bool,
    show_comments: bool,
}

impl<'a> IssuePanel<'a> {
    #[must_use]
    pub fn new(issue: &'a Issue, theme: &'a Theme) -> Self {
        Self {
            issue,
            theme,
            show_dependencies: true,
            show_comments: true,
        }
    }

    pub fn print(&self, ctx: &OutputContext) {
        let mut content = Text::new("");

        // Header: ID and Status badges
        content.append_styled(&format!("{}  ", self.issue.id), self.theme.issue_id.clone());
        content.append_styled(
            &format!("[P{}]  ", self.issue.priority.0),
            self.theme.priority_style(self.issue.priority),
        );
        content.append_styled(
            &format!("{}  ", self.issue.status),
            self.theme.status_style(&self.issue.status),
        );
        content.append_styled(
            &format!("{}\n\n", self.issue.issue_type),
            self.theme.type_style(&self.issue.issue_type),
        );

        // Title
        content.append_styled(&self.issue.title, self.theme.issue_title.clone());
        content.append("\n");

        // Description
        if let Some(ref desc) = self.issue.description {
            content.append("\n");
            content.append_styled(desc, self.theme.issue_description.clone());
            content.append("\n");
        }

        // Metadata section
        content.append_styled(
            "\n───────────────────────────────────\n",
            self.theme.dimmed.clone(),
        );

        // Assignee
        if let Some(ref assignee) = self.issue.assignee {
            content.append_styled("Assignee: ", self.theme.dimmed.clone());
            content.append_styled(&format!("{}\n", assignee), self.theme.username.clone());
        }

        // Labels
        if !self.issue.labels.is_empty() {
            content.append_styled("Labels:   ", self.theme.dimmed.clone());
            for (i, label) in self.issue.labels.iter().enumerate() {
                if i > 0 {
                    content.append(", ");
                }
                content.append_styled(label, self.theme.label.clone());
            }
            content.append("\n");
        }

        // Timestamps
        content.append_styled("Created:  ", self.theme.dimmed.clone());
        content.append_styled(
            &format!("{}\n", self.issue.created_at.format("%Y-%m-%d %H:%M")),
            self.theme.timestamp.clone(),
        );

        content.append_styled("Updated:  ", self.theme.dimmed.clone());
        content.append_styled(
            &format!("{}\n", self.issue.updated_at.format("%Y-%m-%d %H:%M")),
            self.theme.timestamp.clone(),
        );

        // Dependencies
        if self.show_dependencies && !self.issue.dependencies.is_empty() {
            content.append_styled(
                "\n───────────────────────────────────\n",
                self.theme.dimmed.clone(),
            );
            content.append_styled("Dependencies:\n", self.theme.emphasis.clone());
            for dep in &self.issue.dependencies {
                content.append_styled("  → ", self.theme.dimmed.clone());
                content.append_styled(&dep.depends_on_id, self.theme.issue_id.clone());
                content.append(" ");
                content.append_styled(&format!("({})\n", dep.dep_type), self.theme.muted.clone());
            }
        }

        if self.show_comments && !self.issue.comments.is_empty() {
            content.append_styled("\nComments:\n", self.theme.emphasis.clone());
            for comment in &self.issue.comments {
                content.append("  ");
                content.append_styled(
                    &comment.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
                    self.theme.timestamp.clone(),
                );
                content.append(" ");
                content.append_styled(&comment.author, self.theme.username.clone());
                content.append_styled(": ", self.theme.dimmed.clone());
                content.append_styled(&comment.body, self.theme.comment.clone());
                content.append("\n");
            }
        }

        // Build and print panel
        let panel = Panel::from_rich_text(&content, 80)
            .title(Text::styled(&self.issue.id, self.theme.panel_title.clone()))
            .box_style(self.theme.box_style)
            .border_style(self.theme.panel_border.clone());

        ctx.render(&panel);
    }
}
