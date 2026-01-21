use crate::format::truncate_title;
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
    pub created: bool,
    pub updated: bool,
    pub context: bool,
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
            context: false,
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
        }
    }

    #[must_use]
    pub fn width(mut self, width: Option<usize>) -> Self {
        self.width = width;
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

        let title_max_width = if let Some(w) = self.width {
            // Reserve ~100 chars for other columns (conservative) or min 60
            w.saturating_sub(100).max(60)
        } else {
            60
        };

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
        if self.columns.context {
            table = table.with_column(Column::new("Context").min_width(20).max_width(60));
        }

        // Add rows
        for issue in self.issues {
            let mut cells: Vec<Cell> = vec![];

            if self.columns.id {
                cells.push(Cell::new(Text::new(&issue.id)).style(self.theme.issue_id.clone()));
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
                let title = truncate_title(&issue.title, title_max_width);
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
