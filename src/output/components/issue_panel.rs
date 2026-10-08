use crate::format::contains_markdown;
use crate::format::{
    IssueDetails, IssueWithDependencyMetadata, format_status_label, format_type_label,
    sanitize_terminal_inline, sanitize_terminal_text,
};
use crate::model::{Comment, Dependency, Issue};
use crate::output::{OutputContext, OutputMode, Theme};
use rich_rust::cells::cell_len;
use rich_rust::prelude::*;
use rich_rust::renderables::markdown::Markdown;

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

        // Description. In Rich mode a description that uses markdown is
        // rendered (headings, emphasis, code, lists, links) at the panel's
        // inner width; plain prose and every non-Rich mode keep the verbatim
        // text so Plain/JSON output is unchanged.
        if let Some(ref desc) = self.issue.description {
            content.append("\n");
            let sanitized = sanitize_terminal_text(desc);
            if ctx.mode() == OutputMode::Rich && contains_markdown(&sanitized) {
                let inner_width = ctx.width().saturating_sub(4).max(1);
                content.append_text(&markdown_description_text(
                    sanitized.as_ref(),
                    inner_width,
                    wrap,
                    self.theme,
                ));
            } else {
                content.append_styled(sanitized.as_ref(), self.theme.issue_description.clone());
            }
            content.append("\n");
        }

        self.append_workflow_fields(&mut content);

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

    fn append_workflow_fields(&self, content: &mut Text) {
        for (heading, body) in [
            ("Prerequisites", self.issue.prerequisites.as_deref()),
            (
                "Acceptance Criteria",
                self.issue.acceptance_criteria.as_deref(),
            ),
        ] {
            if let Some(body) = body.filter(|body| !body.is_empty()) {
                content.append_styled(&format!("\n{heading}:\n"), self.theme.emphasis.clone());
                content.append_styled(
                    sanitize_terminal_text(body).as_ref(),
                    self.theme.issue_description.clone(),
                );
                content.append("\n");
            }
        }
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

/// Narrowest body a hanging indent may leave. Below this a deeply nested
/// item wraps at the full width instead of into a one-word column.
const MIN_HANGING_BODY_WIDTH: usize = 8;

/// Style handed to `rich_rust`'s Markdown renderer for inline code. The
/// renderer pads every code span to ` code ` (GitHub #529); a true color no
/// other renderer style uses lets [`markdown_description_text`] find exactly
/// those segments, drop the padding before anything measures or wraps them,
/// and restyle them with the theme's `inline_code`.
fn inline_code_sentinel() -> Style {
    Style::new().color(Color::from_rgb(1, 2, 3))
}

/// Render a Markdown description into panel text `width` cells wide.
///
/// Inline code loses the renderer's padding and takes the theme's inline-code
/// style (GitHub #529). With `wrap`, a line wider than `width` wraps under its
/// own text: continuation lines repeat the line's indentation and blockquote
/// bars and replace its list marker with blanks, so a wrapped bullet keeps its
/// hanging indent (GitHub #530). Lines that fit are returned unchanged, so the
/// panel's later wrap never splits them again.
fn markdown_description_text(source: &str, width: usize, wrap: bool, theme: &Theme) -> Text {
    let sentinel = inline_code_sentinel();
    let mut lines = Vec::new();
    let mut current = Text::new("");
    for segment in Markdown::new(source)
        .code_style(sentinel.clone())
        .render(width)
    {
        if segment.is_control() {
            continue;
        }
        let is_code = segment.style.as_ref() == Some(&sentinel);
        let text = segment.text.as_ref();
        let (text, style) = if is_code {
            (
                text.strip_prefix(' ')
                    .and_then(|code| code.strip_suffix(' '))
                    .unwrap_or(text),
                Some(theme.inline_code.clone()),
            )
        } else {
            (text, segment.style)
        };
        for (idx, part) in text.split('\n').enumerate() {
            if idx > 0 {
                lines.push(std::mem::replace(&mut current, Text::new("")));
            }
            match &style {
                Some(style) if !part.is_empty() => current.append_styled(part, style.clone()),
                _ => current.append(part),
            }
        }
    }
    lines.push(current);

    let mut rendered = Text::new("");
    for (idx, line) in lines.iter().enumerate() {
        if idx > 0 {
            rendered.append("\n");
        }
        // The renderer pads each line to `width`; padding is not content and
        // would otherwise count against the wrap below.
        let line = line.slice(0, line.plain().trim_end_matches(' ').chars().count());
        if !wrap || line.cell_len() <= width {
            rendered.append_text(&line);
            continue;
        }
        let (prefix_chars, continuation) = markdown_hanging_prefix(&line);
        let body_width = width.saturating_sub(continuation.cell_len());
        if body_width < MIN_HANGING_BODY_WIDTH {
            rendered.append_text(&line);
            continue;
        }
        let body = line.slice(prefix_chars, line.len());
        for (piece_idx, piece) in body.wrap(body_width).iter().enumerate() {
            if piece_idx == 0 {
                rendered.append_text(&line.slice(0, prefix_chars));
            } else {
                rendered.append("\n");
                rendered.append_text(&continuation);
            }
            rendered.append_text(piece);
        }
    }
    rendered
}

/// Split a rendered Markdown line into its structural prefix (indentation,
/// blockquote bars, one list marker and an optional task checkbox) and the
/// prefix its wrapped continuation lines carry: the same width, with bars
/// kept and markers blanked. Returns the prefix length in chars.
fn markdown_hanging_prefix(line: &Text) -> (usize, Text) {
    let chars: Vec<char> = line.plain().chars().collect();
    let mut idx = 0;
    let mut continuation = Text::new("");
    let mut marker_seen = false;
    while let Some(&ch) = chars.get(idx) {
        let followed_by_space = chars.get(idx + 1) == Some(&' ');
        if ch == ' ' {
            continuation.append(" ");
            idx += 1;
        } else if ch == '│' && followed_by_space && !marker_seen {
            continuation.append_text(&line.slice(idx, idx + 2));
            idx += 2;
        } else if matches!(ch, '•' | '☐' | '☑') && followed_by_space {
            marker_seen = true;
            continuation.append(&" ".repeat(cell_len(&ch.to_string()) + 1));
            idx += 2;
        } else if ch.is_ascii_digit() && !marker_seen {
            let digits = chars[idx..]
                .iter()
                .take_while(|digit| digit.is_ascii_digit())
                .count();
            if chars.get(idx + digits) != Some(&'.') || chars.get(idx + digits + 1) != Some(&' ') {
                break;
            }
            marker_seen = true;
            continuation.append(&" ".repeat(digits + 2));
            idx += digits + 2;
        } else {
            break;
        }
    }
    (idx, continuation)
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
    use super::{
        IssuePanel, dependency_arrow, markdown_description_text, render_dependency_list,
        render_dependency_refs,
    };
    use crate::format::IssueWithDependencyMetadata;
    use crate::model::{Dependency, DependencyType, Issue, Priority, Status};
    use crate::output::Theme;
    use chrono::Utc;
    use rich_rust::prelude::Text;

    fn markdown_lines(source: &str, width: usize, wrap: bool) -> Vec<String> {
        let mut lines: Vec<String> =
            markdown_description_text(source, width, wrap, &Theme::default())
                .plain()
                .lines()
                .map(str::to_string)
                .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines
    }

    #[test]
    fn markdown_inline_code_has_no_padding_and_uses_theme_style() {
        let theme = Theme::default();
        let text =
            markdown_description_text("run `make build` to build, then `x`.", 80, true, &theme);
        assert_eq!(text.plain().trim_end(), "run make build to build, then x.");
        let code_spans: Vec<String> = text
            .spans()
            .iter()
            .filter(|span| span.style == theme.inline_code)
            .map(|span| text.slice(span.start, span.end).plain().to_string())
            .collect();
        assert_eq!(code_spans, vec!["make build", "x"]);
    }

    #[test]
    fn markdown_wrapped_code_span_never_leaves_a_padding_only_line() {
        // Without the fix the padded span " make build " could wrap so that a
        // line held only its padding (GitHub #529).
        for width in 10..30 {
            for line in markdown_lines("aaaa bbbb `make build` cccc", width, true) {
                assert!(line.chars().count() <= width, "{line:?} exceeds {width}");
                assert!(!line.trim().is_empty() || line.is_empty(), "{line:?}");
            }
        }
    }

    #[test]
    fn markdown_wrapped_bullet_keeps_hanging_indent() {
        // The exact shape GitHub #530 asks for.
        let lines = markdown_lines("* blah blah blah blah blah", 18, true);
        assert_eq!(lines, vec!["  • blah blah blah", "    blah blah"]);

        let ordered = markdown_lines("1. alpha beta gamma delta epsilon", 16, true);
        assert_eq!(ordered[0], "  1. alpha beta ");
        assert!(ordered[1..].iter().all(|line| line.starts_with("     ")));
        assert!(ordered[1..].iter().all(|line| !line.trim().is_empty()));

        let nested = markdown_lines("- outer\n  - inner words wrap here nicely", 20, true);
        assert_eq!(nested[0], "  • outer");
        assert!(nested[1].starts_with("    • inner"));
        assert!(nested[2..].iter().all(|line| line.starts_with("      ")));
    }

    #[test]
    fn markdown_wrapped_task_item_and_quote_keep_their_prefix() {
        let task = markdown_lines("- [ ] write the regression test first", 20, true);
        assert!(task[0].starts_with("  • ☐ write"));
        assert!(task[1..].iter().all(|line| line.starts_with("      ")));

        let quote = markdown_lines("> one two three four five six seven", 14, true);
        assert!(quote.len() > 1);
        assert!(quote.iter().all(|line| line.starts_with("│ ")), "{quote:?}");
    }

    #[test]
    fn markdown_lines_that_fit_or_no_wrap_are_unchanged() {
        assert_eq!(
            markdown_lines("- short\n- items", 40, true),
            vec!["  • short", "  • items"]
        );
        // --no-wrap leaves long lines alone for the terminal to handle.
        assert_eq!(
            markdown_lines("* blah blah blah blah blah", 18, false),
            vec!["  • blah blah blah blah blah"]
        );
        // A plain paragraph wraps at the full width with no indent.
        let para = markdown_lines("**word** one two three four five", 12, true);
        assert!(para.iter().skip(1).all(|line| !line.starts_with(' ')));
    }

    #[test]
    fn workflow_fields_are_distinct_and_terminal_safe() {
        let issue = Issue {
            prerequisites: Some("- [x] access granted\n- [ ] review\x1b[2J\x07".to_string()),
            acceptance_criteria: Some("- [ ] ship feature".to_string()),
            ..Issue::default()
        };
        let original = issue.clone();
        let theme = Theme::default();
        let panel = IssuePanel::new(&issue, &theme);
        let mut content = Text::new("");
        panel.append_workflow_fields(&mut content);
        assert_eq!(
            content.plain(),
            "\nPrerequisites:\n- [x] access granted\n- [ ] review\\u{1b}[2J\\u{7}\n\nAcceptance Criteria:\n- [ ] ship feature\n"
        );
        assert_eq!(issue.prerequisites, original.prerequisites);
        assert_eq!(issue.acceptance_criteria, original.acceptance_criteria);
        assert!(!content.plain().contains("Dependencies:"));

        let empty = Issue {
            prerequisites: Some(String::new()),
            ..Issue::default()
        };
        let mut content = Text::new("");
        IssuePanel::new(&empty, &theme).append_workflow_fields(&mut content);
        assert!(content.plain().is_empty());
    }

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
