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
    show_blocked: bool,
}

#[derive(Default, Clone)]
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
    pub fn compact() -> Self {
        Self {
            id: true,
            priority: true,
            issue_type: true,
            title: true,
            ..Default::default()
        }
    }

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
    pub fn new(issues: &'a [Issue], theme: &'a Theme) -> Self {
        Self {
            issues,
            theme,
            columns: IssueTableColumns::standard(),
            title: None,
            show_blocked: false,
        }
    }

    pub fn columns(mut self, columns: IssueTableColumns) -> Self {
        self.columns = columns;
        self
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn show_blocked(mut self, show: bool) -> Self {
        self.show_blocked = show;
        self
    }

    pub fn build(&self) -> Table {
        let mut table = Table::new();
        // .border_style(self.theme.table_border.clone())
        // .header_style(self.theme.table_header.clone());

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
                cells.push(Cell::new(Text::new(&issue.id)));
            }
            if self.columns.priority {
                cells.push(Cell::new(Text::new(format!("P{}", issue.priority.0))));
            }
            if self.columns.status {
                cells.push(Cell::new(Text::new(issue.status.to_string())));
            }
            if self.columns.issue_type {
                cells.push(Cell::new(Text::new(issue.issue_type.to_string())));
            }
            if self.columns.title {
                let mut title = issue.title.clone();
                if title.len() > 57 {
                    title.truncate(57);
                    title.push_str("...");
                }
                cells.push(Cell::new(Text::new(title)));
            }
            if self.columns.assignee {
                cells.push(Cell::new(Text::new(
                    issue.assignee.clone().unwrap_or_default(),
                )));
            }
            if self.columns.labels {
                cells.push(Cell::new(Text::new(issue.labels.join(", "))));
            }
            if self.columns.created {
                cells.push(Cell::new(Text::new(
                    issue.created_at.format("%Y-%m-%d").to_string(),
                )));
            }
            if self.columns.updated {
                cells.push(Cell::new(Text::new(
                    issue.updated_at.format("%Y-%m-%d").to_string(),
                )));
            }

            table.add_row(Row::new(cells));
        }

        table
    }
}
