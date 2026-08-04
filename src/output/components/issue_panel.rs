use crate::format::{
    IssueDetails, IssueWithDependencyMetadata, format_status_label, format_type_label,
    sanitize_terminal_inline, sanitize_terminal_text,
};
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

    pub fn print(&self, ctx: &OutputContext, wrap: bool) {
        let mut content = Text::new("");
        let issue_id = sanitize_terminal_inline(&self.issue.id);

        // Header: ID and Status badges
        content.append_styled(
            &format!("{}  ", issue_id.as_ref()),
            self.theme.issue_id.clone(),
        );
        content.append_styled(
            &format!("[P{}]  ", self.issue.priority.0),
            self.theme.priority_style(self.issue.priority),
        );
        content.append_styled(
            &format!("{}  ", format_status_label(&self.issue.status, false)),
            self.theme.status_style(&self.issue.status),
        );
        content.append_styled(
            &format!("{}\n\n", format_type_label(&self.issue.issue_type)),
            self.theme.type_style(&self.issue.issue_type),
        );

        // Title
        content.append_styled(
            sanitize_terminal_inline(&self.issue.title).as_ref(),
            self.theme.issue_title.clone(),
        );
        content.append("\n");

        // Description
        if let Some(ref desc) = self.issue.description {
            content.append("\n");
            content.append_styled(
                sanitize_terminal_text(desc).as_ref(),
                self.theme.issue_description.clone(),
            );
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
            content.append_styled(
                &format!("{}\n", sanitize_terminal_inline(assignee)),
                self.theme.username.clone(),
            );
        }

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
                content.append_styled(
                    sanitize_terminal_inline(label).as_ref(),
                    self.theme.label.clone(),
                );
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

        self.append_rollup(&mut content);
        self.append_relationships(&mut content);

        // Comments
        let comments: &[Comment] = self
            .details
            .map_or(self.issue.comments.as_slice(), |d| d.comments.as_slice());
        self.append_comments(&mut content, comments);

        // Build and print panel — always use terminal width so descriptions
        // are never silently truncated (issue #91).
        let panel_width = ctx.width();
        let content = if wrap {
            wrap_rich_text(&content, panel_width)
        } else {
            content
        };
        let panel = Panel::from_rich_text(&content, panel_width)
            .title(Text::styled(
                issue_id.into_owned(),
                self.theme.panel_title.clone(),
            ))
            .box_style(self.theme.box_style)
            .border_style(self.theme.panel_border.clone());

        ctx.render(&panel);
    }

    /// Render the derived parent-child subtree rollup (GitHub #384 phase 3).
    /// The issue's own status badge stays authoritative; this line reports
    /// what the subtree beneath it is doing.
    fn append_rollup(&self, content: &mut Text) {
        let Some(rollup) = self.details.and_then(|details| details.rollup.as_ref()) else {
            return;
        };
        let breakdown = rollup
            .descendants
            .iter()
            .map(|(status, count)| format!("{count} {}", sanitize_terminal_inline(status)))
            .collect::<Vec<_>>()
            .join(", ");
        let rollup_status = rollup
            .status
            .parse::<crate::model::Status>()
            .unwrap_or_default();
        content.append_styled("Rollup:   ", self.theme.dimmed.clone());
        content.append_styled(
            sanitize_terminal_inline(&rollup.status).as_ref(),
            self.theme.status_style(&rollup_status),
        );
        content.append_styled(&format!(" ({breakdown})\n"), self.theme.dimmed.clone());
    }

    fn append_relationships(&self, content: &mut Text) {
        if self.show_dependencies {
            if let Some(details) = self.details {
                render_dependency_list(
                    "Dependencies",
                    &details.dependencies,
                    content,
                    self.theme,
                    false,
                );
            } else if !self.issue.dependencies.is_empty() {
                render_dependency_refs(&self.issue.dependencies, content, self.theme);
            }
        }

        if self.show_dependents
            && let Some(details) = self.details
        {
            render_dependency_list("Dependents", &details.dependents, content, self.theme, true);
        }
    }

    fn append_comments(&self, content: &mut Text, comments: &[Comment]) {
        if !self.show_comments || comments.is_empty() {
            return;
        }

        content.append_styled("\nComments:\n", self.theme.emphasis.clone());
        for comment in comments {
            content.append("  ");
            content.append_styled(
                &comment.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
                self.theme.timestamp.clone(),
            );
            content.append(" ");
            content.append_styled(
                sanitize_terminal_inline(&comment.author).as_ref(),
                self.theme.username.clone(),
            );
            content.append_styled(": ", self.theme.dimmed.clone());
            content.append_styled(
                sanitize_terminal_text(&comment.body).as_ref(),
                self.theme.comment.clone(),
            );
            content.append("\n");
        }
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
    is_dependent: bool,
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
        content.append_styled(dependency_arrow(is_dependent), theme.dimmed.clone());
        content.append_styled(
            sanitize_terminal_inline(&dep.id).as_ref(),
            theme.issue_id.clone(),
        );
        content.append(" ");
        content.append_styled(
            &format!("[{}]", format_status_label(&dep.status, false)),
            theme.status_style(&dep.status),
        );
        content.append(" ");
        content.append_styled(
            sanitize_terminal_inline(&dep.title).as_ref(),
            theme.issue_title.clone(),
        );
        content.append(" ");
        content.append_styled(
            &format!("({})", sanitize_terminal_inline(dep.dep_type.as_str())),
            theme.muted.clone(),
        );
        content.append("\n");
    }
}

fn dependency_arrow(is_dependent: bool) -> &'static str {
    if is_dependent { "  ← " } else { "  → " }
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
        content.append_styled(
            sanitize_terminal_inline(&dep.depends_on_id).as_ref(),
            theme.issue_id.clone(),
        );
        content.append(" ");
        content.append_styled(
            &format!("({})", sanitize_terminal_inline(dep.dep_type.as_str())),
            theme.muted.clone(),
        );
        content.append("\n");
    }
}

#[cfg(test)]
mod tests {
    use super::{dependency_arrow, render_dependency_list, render_dependency_refs};
    use crate::format::IssueWithDependencyMetadata;
    use crate::model::{Dependency, DependencyType, Priority, Status};
    use crate::output::Theme;
    use chrono::Utc;
    use rich_rust::prelude::Text;

    #[test]
    fn test_dependency_arrow_tracks_direction() {
        assert_eq!(dependency_arrow(false), "  → ");
        assert_eq!(dependency_arrow(true), "  ← ");
    }

    #[test]
    fn dependency_rendering_sanitizes_ids_and_types() {
        let theme = Theme::default();
        let metadata_deps = vec![IssueWithDependencyMetadata {
            id: "bd-dep\x1b[2J".to_string(),
            title: "Dependency title".to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            dep_type: "blocks\x07".to_string(),
        }];
        let mut metadata_content = Text::new("");
        render_dependency_list(
            "Dependencies",
            &metadata_deps,
            &mut metadata_content,
            &theme,
            false,
        );

        let raw_deps = vec![Dependency {
            issue_id: "bd-source".to_string(),
            depends_on_id: "bd-target\x1b]52;c;bad\x07".to_string(),
            dep_type: DependencyType::Custom("custom\x08type".to_string()),
            created_at: Utc::now(),
            created_by: None,
            metadata: None,
            thread_id: None,
        }];
        let mut raw_content = Text::new("");
        render_dependency_refs(&raw_deps, &mut raw_content, &theme);

        for rendered in [metadata_content.plain(), raw_content.plain()] {
            assert!(!rendered.contains('\x1b'));
            assert!(!rendered.contains('\x07'));
            assert!(!rendered.contains('\x08'));
        }
        assert!(metadata_content.plain().contains("bd-dep\\u{1b}[2J"));
        assert!(metadata_content.plain().contains("blocks\\u{7}"));
        assert!(
            raw_content
                .plain()
                .contains("bd-target\\u{1b}]52;c;bad\\u{7}")
        );
        assert!(raw_content.plain().contains("custom\\u{8}type"));
    }
}
