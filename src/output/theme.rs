use rich_rust::prelude::*;
use crate::model::{Status, IssueType, Priority};

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
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            success: Style::default(),
            error: Style::default(),
            warning: Style::default(),
            info: Style::default(),
            dimmed: Style::default(),
            accent: Style::default(),
            highlight: Style::default(),
            muted: Style::default(),
            emphasis: Style::default(),

            issue_id: Style::default(),
            issue_title: Style::default(),
            issue_description: Style::default(),

            status_open: Style::default(),
            status_in_progress: Style::default(),
            status_blocked: Style::default(),
            status_deferred: Style::default(),
            status_closed: Style::default(),

            priority_critical: Style::default(),
            priority_high: Style::default(),
            priority_medium: Style::default(),
            priority_low: Style::default(),
            priority_backlog: Style::default(),

            type_task: Style::default(),
            type_bug: Style::default(),
            type_feature: Style::default(),
            type_epic: Style::default(),
            type_chore: Style::default(),
            type_docs: Style::default(),
            type_question: Style::default(),

            table_header: Style::default(),
            table_border: Style::default(),
            panel_title: Style::default(),
            panel_border: Style::default(),
            section: Style::default(),
            label: Style::default(),
            timestamp: Style::default(),
            username: Style::default(),
            comment: Style::default(),
        }
    }
}

impl Theme {
    pub fn status_style(&self, _status: &Status) -> Style {
        Style::default()
    }

    pub fn priority_style(&self, _priority: Priority) -> Style {
        Style::default()
    }

    pub fn type_style(&self, _issue_type: &IssueType) -> Style {
        Style::default()
    }
}
