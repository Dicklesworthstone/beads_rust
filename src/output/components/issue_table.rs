use crate::model::Issue;
use crate::output::Theme;
use rich_rust::prelude::*;
use rich_rust::renderables::Cell;

/// Renders a list of issues as a beautiful table.
pub struct IssueTable<'a> {
    issues: &'a [Issue],
    theme: &'a Theme,
    columns: IssueTableColumns,
    title: Option<String>,
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
        }
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
    pub fn build(&self) -> Table {
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
            table = table.with_column(Column::new("Title").min_width(20).max_width(60));
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
                let mut title = issue.title.clone();
                if title.len() > 57 {
                    title.truncate(57);
                    title.push_str("...");
                }
                cells.push(Cell::new(Text::new(title)).style(self.theme.issue_title.clone()));
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

            table.add_row(Row::new(cells));
        }

        table
    }
}
