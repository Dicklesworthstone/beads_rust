use crate::format::{format_issue_age_field, truncate_title};
use crate::model::Issue;
use crate::output::Theme;
use regex::{Regex, RegexBuilder};
use rich_rust::prelude::*;
use rich_rust::renderables::Cell;
use std::collections::HashMap;

/// Renders a list of issues as a beautiful table.
pub struct IssueTable<'a> {
    issues: &'a [Issue],
    theme: &'a Theme,
    columns: IssueTableColumns,
    title: Option<String>,
    highlight_query: Option<String>,
    context_snippets: Option<HashMap<String, String>>,
    width: Option<usize>,
    wrap: bool,
}

#[derive(Default, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct IssueTableColumns {
    pub id: bool,
    pub priority: bool,
    pub status: bool,
    pub issue_type: bool,
    pub title: bool,
    pub assignee: bool,
    pub labels: bool,
    /// Absolute `Created`/`Updated` date columns (`%Y-%m-%d`, one
    /// column each). Superseded by `age` for `bd list`, which uses a
    /// single combined compact-age column instead; kept here for
    /// other callers that still want absolute dates.
    pub created: bool,
    pub updated: bool,
    /// Combined compact age column, e.g. `5d/2h` (created/updated) —
    /// same presentation and dedupe rule as
    /// [`crate::format::format_issue_age_field`], so the rich table
    /// and the plain-text line agree. Mutually exclusive with
    /// `created`/`updated` in practice (nothing stops enabling both,
    /// but no caller does).
    pub age: bool,
    pub context: bool,
    /// When set (and `id` is enabled), color the ID cell by issue
    /// type ([`Theme::type_style`]) instead of the flat
    /// [`Theme::issue_id`] color, and skip the separate `Type`
    /// column — the color itself carries the type signal. Only
    /// meaningful when color is actually going to render (callers
    /// should gate this on their own "is color available" check;
    /// `IssueTable` doesn't second-guess it).
    pub color_id_by_type: bool,
}

impl IssueTableColumns {
    #[must_use]
    pub fn compact() -> Self {
        Self {
            id: true,
            priority: true,
            issue_type: true,
            title: true,
            ..Default::default()
        }
    }

    #[must_use]
    pub fn standard() -> Self {
        Self {
            id: true,
            priority: true,
            status: true,
            issue_type: true,
            title: true,
            assignee: true,
            ..Default::default()
        }
    }

    #[must_use]
    pub fn full() -> Self {
        Self {
            id: true,
            priority: true,
            status: true,
            issue_type: true,
            title: true,
            assignee: true,
            labels: true,
            created: true,
            updated: true,
            age: false,
            context: false,
            color_id_by_type: false,
        }
    }
}

impl<'a> IssueTable<'a> {
    #[must_use]
    pub fn new(issues: &'a [Issue], theme: &'a Theme) -> Self {
        Self {
            issues,
            theme,
            columns: IssueTableColumns::standard(),
            title: None,
            highlight_query: None,
            context_snippets: None,
            width: None,
            wrap: false,
        }
    }

    #[must_use]
    pub fn width(mut self, width: Option<usize>) -> Self {
        self.width = width;
        self
    }

    #[must_use]
    pub fn wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    #[must_use]
    pub fn columns(mut self, columns: IssueTableColumns) -> Self {
        self.columns = columns;
        self
    }

    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    #[must_use]
    pub fn highlight_query(mut self, query: impl Into<String>) -> Self {
        let query = query.into();
        if !query.trim().is_empty() {
            self.highlight_query = Some(query);
        }
        self
    }

    #[must_use]
    pub fn context_snippets(mut self, snippets: HashMap<String, String>) -> Self {
        if !snippets.is_empty() {
            self.context_snippets = Some(snippets);
        }
        self
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn build(&self) -> Table {
        let highlight_regex = self
            .highlight_query
            .as_deref()
            .and_then(build_highlight_regex);

        // Reserve space for whichever other columns are actually
        // enabled (their min/fixed width plus ~3 chars of border and
        // padding each), so the Title column gets whatever's left —
        // dropping a column (e.g. Type, Assignee) or shrinking one
        // (e.g. two 10-wide date columns collapsed into one 9-wide
        // Age column) now visibly hands that space back to titles
        // instead of it sitting reserved-but-unused behind a fixed
        // budget tuned for the old, wider column set.
        let mut reserved = 4; // outer table border/padding
        if self.columns.id {
            reserved += 10 + 3;
        }
        if self.columns.priority {
            reserved += 3 + 3;
        }
        if self.columns.status {
            reserved += 8 + 3;
        }
        if self.columns.issue_type {
            reserved += 7 + 3;
        }
        if self.columns.assignee {
            reserved += 20 + 3;
        }
        if self.columns.labels {
            reserved += 15 + 3;
        }
        if self.columns.created {
            reserved += 10 + 3;
        }
        if self.columns.updated {
            reserved += 10 + 3;
        }
        if self.columns.age {
            reserved += 7 + 3;
        }
        if self.columns.context {
            reserved += 20 + 3;
        }
        let title_max_width = self
            .width
            .map_or(60, |w| w.saturating_sub(reserved).max(20));

        let mut table = Table::new()
            .box_style(self.theme.box_style)
            .border_style(self.theme.table_border.clone())
            .header_style(self.theme.table_header.clone());

        if let Some(ref title) = self.title {
            table = table.title(Text::new(title));
        }

        // Add columns based on config
        if self.columns.id {
            table = table.with_column(Column::new("ID").min_width(10));
        }
        if self.columns.priority {
            table = table.with_column(Column::new("P").justify(JustifyMethod::Center).width(3));
        }
        if self.columns.status {
            table = table.with_column(Column::new("Status").min_width(8));
        }
        if self.columns.issue_type {
            table = table.with_column(Column::new("Type").min_width(7));
        }
        if self.columns.title {
            table = table.with_column(
                Column::new("Title")
                    .min_width(20)
                    .max_width(title_max_width),
            );
        }
        if self.columns.assignee {
            table = table.with_column(Column::new("Assignee").max_width(20));
        }
        if self.columns.labels {
            table = table.with_column(Column::new("Labels").max_width(30));
        }
        if self.columns.created {
            table = table.with_column(Column::new("Created").width(10));
        }
        if self.columns.updated {
            table = table.with_column(Column::new("Updated").width(10));
        }
        if self.columns.age {
            // "created/updated" compact ages, e.g. `5d/2h` — up to
            // ~7-8 chars in the common case, matching the padded
            // width used by the plain-text line.
            table = table.with_column(Column::new("Age").width(7));
        }
        if self.columns.context {
            table = table.with_column(Column::new("Context").min_width(20).max_width(60));
        }

        // Add rows
        for issue in self.issues {
            let mut cells: Vec<Cell> = vec![];

            if self.columns.id {
                // When the Type column is dropped in favor of coloring
                // the ID by issue type, the ID cell carries that
                // signal; otherwise it gets the theme's flat ID color.
                let id_style = if self.columns.color_id_by_type {
                    self.theme.type_style(&issue.issue_type)
                } else {
                    self.theme.issue_id.clone()
                };
                cells.push(Cell::new(Text::new(&issue.id)).style(id_style));
            }
            if self.columns.priority {
                cells.push(
                    Cell::new(Text::new(format!("P{}", issue.priority.0)))
                        .style(self.theme.priority_style(issue.priority)),
                );
            }
            if self.columns.status {
                cells.push(
                    Cell::new(Text::new(issue.status.to_string()))
                        .style(self.theme.status_style(&issue.status)),
                );
            }
            if self.columns.issue_type {
                cells.push(
                    Cell::new(Text::new(issue.issue_type.to_string()))
                        .style(self.theme.type_style(&issue.issue_type)),
                );
            }
            if self.columns.title {
                let title = if self.wrap {
                    issue.title.clone()
                } else {
                    truncate_title(&issue.title, title_max_width)
                };
                let title_text = highlight_text(&title, highlight_regex.as_ref(), self.theme);
                cells.push(Cell::new(title_text).style(self.theme.issue_title.clone()));
            }
            if self.columns.assignee {
                cells.push(
                    Cell::new(Text::new(issue.assignee.clone().unwrap_or_default()))
                        .style(self.theme.username.clone()),
                );
            }
            if self.columns.labels {
                cells.push(
                    Cell::new(Text::new(issue.labels.join(", "))).style(self.theme.label.clone()),
                );
            }
            if self.columns.created {
                cells.push(
                    Cell::new(Text::new(issue.created_at.format("%Y-%m-%d").to_string()))
                        .style(self.theme.timestamp.clone()),
                );
            }
            if self.columns.updated {
                cells.push(
                    Cell::new(Text::new(issue.updated_at.format("%Y-%m-%d").to_string()))
                        .style(self.theme.timestamp.clone()),
                );
            }
            if self.columns.age {
                cells.push(
                    Cell::new(Text::new(format_issue_age_field(issue)))
                        .style(self.theme.timestamp.clone()),
                );
            }
            if self.columns.context {
                let snippet = self
                    .context_snippets
                    .as_ref()
                    .and_then(|snippets| snippets.get(&issue.id))
                    .map_or("", String::as_str);
                let snippet_text = highlight_text(snippet, highlight_regex.as_ref(), self.theme);
                cells.push(Cell::new(snippet_text).style(self.theme.muted.clone()));
            }

            table.add_row(Row::new(cells));
        }

        table
    }
}

fn build_highlight_regex(query: &str) -> Option<Regex> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    let pattern = regex::escape(trimmed);
    RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
        .ok()
}

fn highlight_text(text: &str, regex: Option<&Regex>, theme: &Theme) -> Text {
    let Some(regex) = regex else {
        return Text::new(text);
    };

    let mut rich_text = Text::new("");
    let mut last = 0;
    let mut found = false;

    for matched in regex.find_iter(text) {
        found = true;
        let start = matched.start();
        let end = matched.end();
        if start > last {
            rich_text.append(&text[last..start]);
        }
        rich_text.append_styled(&text[start..end], theme.highlight.clone());
        last = end;
    }

    if !found {
        return Text::new(text);
    }
    if last < text.len() {
        rich_text.append(&text[last..]);
    }

    rich_text
}

#[cfg(test)]
mod tests {
    use crate::format::truncate_title;

    #[test]
    fn test_table_truncation_safe() {
        let title = "😊".repeat(60); // 240 bytes, 60 chars, 120 visual width

        let truncated = truncate_title(&title, 60);

        // Should be safe and shorter than original
        assert!(truncated.chars().count() < 60);
        assert!(truncated.starts_with("😊"));
        assert!(truncated.ends_with("..."));
    }
}
