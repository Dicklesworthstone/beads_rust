//! Theme and color definitions for rich output.

use crate::model::{IssueType, Priority, Status};
use rich_rust::r#box::ROUNDED;
use rich_rust::prelude::*;

fn color(name: &str) -> Color {
    Color::parse(name).unwrap_or_else(|_| panic!("Invalid color name: {name}"))
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub success: Style,
    pub error: Style,
    pub warning: Style,
    pub info: Style,
    pub dimmed: Style,
    pub accent: Style,
    pub highlight: Style,
    pub muted: Style,
    pub emphasis: Style,

    pub issue_id: Style,
    pub issue_title: Style,
    pub issue_description: Style,

    pub status_open: Style,
    pub status_in_progress: Style,
    pub status_blocked: Style,
    pub status_deferred: Style,
    pub status_closed: Style,

    pub priority_critical: Style,
    pub priority_high: Style,
    pub priority_medium: Style,
    pub priority_low: Style,
    pub priority_backlog: Style,

    pub type_task: Style,
    pub type_bug: Style,
    pub type_feature: Style,
    pub type_epic: Style,
    pub type_chore: Style,
    pub type_docs: Style,
    pub type_question: Style,

    pub table_header: Style,
    pub table_border: Style,
    pub panel_title: Style,
    pub panel_border: Style,
    pub section: Style,
    pub label: Style,
    pub timestamp: Style,
    pub username: Style,
    pub comment: Style,

    pub box_style: &'static BoxChars,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            success: Style::new().color(color("green")).bold(),
            error: Style::new().color(color("red")).bold(),
            warning: Style::new().color(color("yellow")).bold(),
            info: Style::new().color(color("blue")),
            dimmed: Style::new().dim(),
            accent: Style::new().color(color("cyan")),
            highlight: Style::new().color(color("magenta")),
            muted: Style::new().color(color("bright_black")),
            emphasis: Style::new().bold(),

            issue_id: Style::new().color(color("cyan")).bold(),
            issue_title: Style::new().bold(),
            issue_description: Style::new(),

            status_open: Style::new().color(color("green")),
            status_in_progress: Style::new().color(color("yellow")).bold(),
            status_blocked: Style::new().color(color("red")),
            status_deferred: Style::new().color(color("blue")).dim(),
            status_closed: Style::new().color(color("bright_black")),

            priority_critical: Style::new().color(color("red")).bold(),
            priority_high: Style::new().color(color("red")),
            priority_medium: Style::new().color(color("yellow")),
            priority_low: Style::new().color(color("green")),
            priority_backlog: Style::new().color(color("bright_black")),

            type_task: Style::new().color(color("blue")),
            type_bug: Style::new().color(color("red")),
            type_feature: Style::new().color(color("green")),
            type_epic: Style::new().color(color("magenta")).bold(),
            type_chore: Style::new().color(color("bright_black")),
            type_docs: Style::new().color(color("cyan")),
            type_question: Style::new().color(color("yellow")),

            table_header: Style::new().bold(),
            table_border: Style::new().color(color("bright_black")),
            panel_title: Style::new().bold(),
            panel_border: Style::new().color(color("bright_black")),
            section: Style::new().color(color("cyan")).bold(),
            label: Style::new().color(color("cyan")).dim(),
            timestamp: Style::new().color(color("bright_black")),
            username: Style::new().color(color("green")),
            comment: Style::new().italic(),

            box_style: &ROUNDED,
        }
    }
}

impl Theme {
    #[must_use]
    pub fn status_style(&self, status: &Status) -> Style {
        match status {
            Status::Open => self.status_open.clone(),
            Status::InProgress => self.status_in_progress.clone(),
            Status::Blocked => self.status_blocked.clone(),
            Status::Deferred => self.status_deferred.clone(),
            Status::Closed => self.status_closed.clone(),
            Status::Tombstone | Status::Custom(_) => self.muted.clone(),
            Status::Pinned => self.highlight.clone(),
        }
    }

    #[must_use]
    pub fn priority_style(&self, priority: Priority) -> Style {
        match priority.0 {
            0 => self.priority_critical.clone(),
            1 => self.priority_high.clone(),
            2 => self.priority_medium.clone(),
            3 => self.priority_low.clone(),
            _ => self.priority_backlog.clone(),
        }
    }

    #[must_use]
    pub fn type_style(&self, issue_type: &IssueType) -> Style {
        match issue_type {
            IssueType::Task => self.type_task.clone(),
            IssueType::Bug => self.type_bug.clone(),
            IssueType::Feature => self.type_feature.clone(),
            IssueType::Epic => self.type_epic.clone(),
            IssueType::Chore => self.type_chore.clone(),
            IssueType::Docs => self.type_docs.clone(),
            IssueType::Question => self.type_question.clone(),
            IssueType::Custom(_) => self.muted.clone(),
        }
    }
}
