use rich_rust::prelude::*;
use crate::model::Issue;
use crate::output::{Theme, OutputContext};

/// Renders a single issue with full details in a styled panel.
pub struct IssuePanel<'a> {
    issue: &'a Issue,
    theme: &'a Theme,
    show_dependencies: bool,
    show_comments: bool,
    show_history: bool,
}

impl<'a> IssuePanel<'a> {
    pub fn new(issue: &'a Issue, theme: &'a Theme) -> Self {
        Self {
            issue,
            theme,
            show_dependencies: true,
            show_comments: true,
            show_history: false,
        }
    }

    pub fn print(&self, ctx: &OutputContext) {
        let mut content = Text::new("");

        // Header: ID and Status badges
        content.append(&format!("{}  ", self.issue.id));
        content.append(&format!("[P{}]  ", self.issue.priority.0));
        content.append(&format!("{}  ", self.issue.status));
        content.append(&format!("{}\n\n", self.issue.issue_type));

        // Title
        content.append(&self.issue.title);
        content.append("\n");

        // Description
        if let Some(ref desc) = self.issue.description {
            content.append("\n");
            content.append(desc);
            content.append("\n");
        }

        // Metadata section
        content.append("\n───────────────────────────────────\n");

        // Assignee
        if let Some(ref assignee) = self.issue.assignee {
            content.append("Assignee: ");
            content.append(&format!("{}\n", assignee));
        }

        // Labels
        if !self.issue.labels.is_empty() {
            content.append("Labels:   ");
            for (i, label) in self.issue.labels.iter().enumerate() {
                if i > 0 {
                    content.append(", ");
                }
                content.append(label);
            }
            content.append("\n");
        }

        // Timestamps
        content.append("Created:  ");
        content.append(&format!("{}\n", self.issue.created_at.format("%Y-%m-%d %H:%M")));

        content.append("Updated:  ");
        content.append(&format!("{}\n", self.issue.updated_at.format("%Y-%m-%d %H:%M")));

        // Dependencies
        if self.show_dependencies && !self.issue.dependencies.is_empty() {
            content.append("\n───────────────────────────────────\n");
            content.append("Dependencies:\n");
            for dep in &self.issue.dependencies {
                content.append(&format!("  → {} ", dep.depends_on_id));
                content.append(&format!("({})\n", dep.dep_type));
            }
        }

        // Build and print panel
        let panel = Panel::from_rich_text(&content, 80) // default width 80
            .title(Text::new(&self.issue.id));
            // .border_style(self.theme.panel_border.clone())
        
        ctx.render(&panel);
    }
}