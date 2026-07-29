use crate::format::{IssueDetails, IssueWithDependencyMetadata};
use crate::model::{Comment, Dependency, Issue};
use crate::output::{OutputContext, Theme};
use rich_rust::prelude::*;

/// Renders a single issue with full details in a styled panel.
pub struct IssuePanel<'a> {
    issue: &'a Issue,
    details: Option<&'a IssueDetails>,
    theme: &'a Theme,
    show_dependencies: bool,
    show_dependents: bool,
    show_comments: bool,
}

impl<'a> IssuePanel<'a> {
    #[must_use]
    pub fn new(issue: &'a Issue, theme: &'a Theme) -> Self {
        Self {
            issue,
            details: None,
            theme,
            show_dependencies: true,
            show_dependents: true,
            show_comments: true,
        }
    }

    #[must_use]
    pub fn from_details(details: &'a IssueDetails, theme: &'a Theme) -> Self {
        Self {
            issue: &details.issue,
            details: Some(details),
            theme,
            show_dependencies: true,
            show_dependents: true,
            show_comments: true,
        }
    }

    #[must_use]
    pub fn show_dependencies(mut self, show: bool) -> Self {
        self.show_dependencies = show;
        self
    }

    #[must_use]
    pub fn show_dependents(mut self, show: bool) -> Self {
        self.show_dependents = show;
        self
    }

    #[must_use]
    pub fn show_comments(mut self, show: bool) -> Self {
        self.show_comments = show;
        self
    }

    /// Appends the "Creator: <agent>" line — the agent (or fallback
    /// user) that created the issue. Only rendered when present:
    /// historical issues predating this field have no value here, and
    /// "unknown" would just be noise. Split out of `print` to keep
    /// that function from growing further (it's already long).
    fn append_creator_line(content: &mut Text, issue: &Issue, theme: &Theme) {
        if let Some(ref created_by) = issue.created_by {
            if !created_by.is_empty() {
                content.append_styled("Creator:  ", theme.dimmed.clone());
                content.append_styled(&format!("{}\n", created_by), theme.username.clone());
            }
        }
    }

    pub fn print(&self, ctx: &OutputContext, wrap: bool) {
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

        // Notes (separate column from description — persisted but
        // previously invisible in the rich panel).
        if let Some(ref notes) = self.issue.notes {
            if !notes.is_empty() {
                content.append_styled("\nNotes:\n", self.theme.emphasis.clone());
                content.append_styled(notes, self.theme.issue_description.clone());
                content.append("\n");
            }
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

        // Owner (distinct from assignee — who's accountable, not who's
        // working it). Only render when explicitly set; no USER env
        // fallback in the rich view since "unknown" adds noise.
        if let Some(ref owner) = self.issue.owner {
            content.append_styled("Owner:    ", self.theme.dimmed.clone());
            content.append_styled(&format!("{}\n", owner), self.theme.username.clone());
        }

        Self::append_creator_line(&mut content, self.issue, self.theme);

        // Labels
        let labels = self
            .details
            .map_or(self.issue.labels.as_slice(), |d| d.labels.as_slice());
        if !labels.is_empty() {
            content.append_styled("Labels:   ", self.theme.dimmed.clone());
            for (i, label) in labels.iter().enumerate() {
                if i > 0 {
                    content.append(", ");
                }
                content.append_styled(label, self.theme.label.clone());
            }
            content.append("\n");
        }

        // External ref (e.g. JIRA-123, GH#42)
        if let Some(ref ext_ref) = self.issue.external_ref {
            if !ext_ref.is_empty() {
                content.append_styled("Ref:      ", self.theme.dimmed.clone());
                content.append_styled(&format!("{}\n", ext_ref), self.theme.timestamp.clone());
            }
        }

        // Due / Deferred / Estimate
        if let Some(due) = self.issue.due_at {
            content.append_styled("Due:      ", self.theme.dimmed.clone());
            content.append_styled(
                &format!("{}\n", due.format("%Y-%m-%d")),
                self.theme.timestamp.clone(),
            );
        }
        if let Some(defer) = self.issue.defer_until {
            content.append_styled("Deferred: ", self.theme.dimmed.clone());
            content.append_styled(
                &format!("{}\n", defer.format("%Y-%m-%d")),
                self.theme.timestamp.clone(),
            );
        }
        if let Some(minutes) = self.issue.estimated_minutes {
            if minutes > 0 {
                let hours = minutes / 60;
                let remaining = minutes % 60;
                let formatted = match (hours, remaining) {
                    (0, m) => format!("{m}m"),
                    (h, 0) => format!("{h}h"),
                    (h, m) => format!("{h}h {m}m"),
                };
                content.append_styled("Estimate: ", self.theme.dimmed.clone());
                content.append_styled(&format!("{formatted}\n"), self.theme.timestamp.clone());
            }
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

        // Closed: <date> (<reason>)  — only shown when the bead is
        // actually closed. close_reason is the why; closed_at is the
        // when. Most user-visible miss before this patch.
        if let Some(closed) = self.issue.closed_at {
            let reason = self.issue.close_reason.as_deref().unwrap_or("closed");
            content.append_styled("Closed:   ", self.theme.dimmed.clone());
            content.append_styled(
                &format!("{} ({reason})\n", closed.format("%Y-%m-%d %H:%M")),
                self.theme.timestamp.clone(),
            );
        }

        // Dependencies / Dependents
        if self.show_dependencies {
            if let Some(details) = self.details {
                render_dependency_list(
                    "Dependencies",
                    &details.dependencies,
                    &mut content,
                    self.theme,
                );
            } else if !self.issue.dependencies.is_empty() {
                render_dependency_refs(&self.issue.dependencies, &mut content, self.theme);
            }
        }

        if self.show_dependents {
            if let Some(details) = self.details {
                render_dependency_list("Dependents", &details.dependents, &mut content, self.theme);
            }
        }

        // Comments
        let comments: &[Comment] = self
            .details
            .map_or(self.issue.comments.as_slice(), |d| d.comments.as_slice());
        if self.show_comments && !comments.is_empty() {
            content.append_styled("\nComments:\n", self.theme.emphasis.clone());
            for comment in comments {
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
        let panel_width = if wrap { ctx.width() } else { 80 };
        let content = if wrap {
            wrap_rich_text(&content, panel_width)
        } else {
            content
        };
        let panel = Panel::from_rich_text(&content, panel_width)
            .title(Text::styled(&self.issue.id, self.theme.panel_title.clone()))
            .box_style(self.theme.box_style)
            .border_style(self.theme.panel_border.clone());

        ctx.render(&panel);
    }
}

fn wrap_rich_text(text: &Text, panel_width: usize) -> Text {
    let content_width = panel_width.saturating_sub(4).max(1);
    let lines = text.wrap(content_width);
    let mut wrapped = Text::new("");
    for (idx, line) in lines.iter().enumerate() {
        if idx > 0 {
            wrapped.append("\n");
        }
        wrapped.append_text(line);
    }
    wrapped
}

fn render_dependency_list(
    title: &str,
    deps: &[IssueWithDependencyMetadata],
    content: &mut Text,
    theme: &Theme,
) {
    if deps.is_empty() {
        return;
    }

    content.append_styled(
        "\n───────────────────────────────────\n",
        theme.dimmed.clone(),
    );
    content.append_styled(&format!("{title}:\n"), theme.emphasis.clone());
    for dep in deps {
        content.append_styled("  → ", theme.dimmed.clone());
        content.append_styled(&dep.id, theme.issue_id.clone());
        content.append(" ");
        content.append_styled(
            &format!("[{}]", dep.status.as_str()),
            theme.status_style(&dep.status),
        );
        content.append(" ");
        content.append_styled(&dep.title, theme.issue_title.clone());
        content.append(" ");
        content.append_styled(&format!("({})", dep.dep_type), theme.muted.clone());
        content.append("\n");
    }
}

fn render_dependency_refs(deps: &[Dependency], content: &mut Text, theme: &Theme) {
    if deps.is_empty() {
        return;
    }

    content.append_styled(
        "\n───────────────────────────────────\n",
        theme.dimmed.clone(),
    );
    content.append_styled("Dependencies:\n", theme.emphasis.clone());
    for dep in deps {
        content.append_styled("  → ", theme.dimmed.clone());
        content.append_styled(&dep.depends_on_id, theme.issue_id.clone());
        content.append(" ");
        content.append_styled(&format!("({})", dep.dep_type), theme.muted.clone());
        content.append("\n");
    }
}
