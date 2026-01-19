//! Theme and color definitions for rich output.
//!
//! These EXTEND the existing color patterns in text.rs:
//! - Status: green (open) → yellow (in_progress) → red (blocked) → gray (closed)
//! - Priority: red+bold (P0) → red → yellow → gray
//! - Type: red (bug), cyan (feature), magenta+bold (epic), etc.
//!
//! This module provides [`Theme`] which wraps these semantics in rich_rust [`Style`] objects.

use crate::model::{IssueType, Priority, Status};
use rich_rust::{Color, Style};

/// Helper to parse a color by name, panicking on invalid colors.
/// Used at theme initialization time for standard color names.
fn color(name: &str) -> Color {
    Color::parse(name).unwrap_or_else(|_| panic!("Invalid color name: {name}"))
}

/// Theme providing consistent styling across rich output components.
///
/// The default theme matches the existing color patterns from `text.rs`
/// for visual consistency.
#[derive(Debug, Clone)]
pub struct Theme {
    // Status colors
    pub status_open: Style,
    pub status_in_progress: Style,
    pub status_blocked: Style,
    pub status_deferred: Style,
    pub status_closed: Style,
    pub status_pinned: Style,

    // Priority colors
    pub priority_critical: Style,
    pub priority_high: Style,
    pub priority_medium: Style,
    pub priority_low: Style,

    // Type colors
    pub type_bug: Style,
    pub type_feature: Style,
    pub type_task: Style,
    pub type_epic: Style,
    pub type_docs: Style,
    pub type_chore: Style,

    // Structural elements
    pub header: Style,
    pub border: Style,
    pub muted: Style,
    pub emphasis: Style,
    pub success: Style,
    pub warning: Style,
    pub error: Style,

    // Issue ID styling
    pub issue_id: Style,
    pub label: Style,
    pub dependency: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self::new()
    }
}

impl Theme {
    /// Create a new theme with default colors matching existing text.rs patterns.
    #[must_use]
    pub fn new() -> Self {
        Self {
            // Status colors - matching text.rs format_status_label
            status_open: Style::new().color(color("green")),
            status_in_progress: Style::new().color(color("yellow")),
            status_blocked: Style::new().color(color("red")),
            status_deferred: Style::new().color(color("blue")),
            status_closed: Style::new().color(color("bright_black")),
            status_pinned: Style::new().color(color("magenta")).bold(),

            // Priority colors - matching text.rs format_priority_label
            priority_critical: Style::new().color(color("red")).bold(),
            priority_high: Style::new().color(color("red")),
            priority_medium: Style::new().color(color("yellow")),
            priority_low: Style::new().color(color("bright_black")),

            // Type colors - matching text.rs format_type_badge_colored
            type_bug: Style::new().color(color("red")),
            type_feature: Style::new().color(color("cyan")),
            type_task: Style::new(),
            type_epic: Style::new().color(color("magenta")).bold(),
            type_docs: Style::new().color(color("blue")),
            type_chore: Style::new().color(color("bright_black")),

            // Structural elements
            header: Style::new().bold(),
            border: Style::new().color(color("bright_black")),
            muted: Style::new().color(color("bright_black")),
            emphasis: Style::new().bold(),
            success: Style::new().color(color("green")),
            warning: Style::new().color(color("yellow")),
            error: Style::new().color(color("red")).bold(),

            // Issue-specific
            issue_id: Style::new().color(color("cyan")),
            label: Style::new().color(color("blue")),
            dependency: Style::new().color(color("magenta")),
        }
    }

    /// Get the style for a given status.
    #[must_use]
    pub fn status_style(&self, status: &Status) -> &Style {
        match status {
            Status::Open => &self.status_open,
            Status::InProgress => &self.status_in_progress,
            Status::Blocked => &self.status_blocked,
            Status::Deferred => &self.status_deferred,
            Status::Closed | Status::Tombstone => &self.status_closed,
            Status::Pinned => &self.status_pinned,
            Status::Custom(_) => &self.muted,
        }
    }

    /// Get the style for a given priority.
    #[must_use]
    pub fn priority_style(&self, priority: &Priority) -> &Style {
        match priority.0 {
            0 => &self.priority_critical,
            1 => &self.priority_high,
            2 => &self.priority_medium,
            3 | 4 => &self.priority_low,
            _ => &self.muted,
        }
    }

    /// Get the style for a given issue type.
    #[must_use]
    pub fn type_style(&self, issue_type: &IssueType) -> &Style {
        match issue_type {
            IssueType::Bug => &self.type_bug,
            IssueType::Feature => &self.type_feature,
            IssueType::Task | IssueType::Custom(_) => &self.type_task,
            IssueType::Epic => &self.type_epic,
            IssueType::Docs | IssueType::Question => &self.type_docs,
            IssueType::Chore => &self.type_chore,
        }
    }

    /// Create a "dark mode" theme variant.
    ///
    /// Uses brighter colors for better visibility on dark backgrounds.
    #[must_use]
    pub fn dark() -> Self {
        Self {
            status_open: Style::new().color(color("bright_green")),
            status_in_progress: Style::new().color(color("bright_yellow")),
            status_blocked: Style::new().color(color("bright_red")),
            status_deferred: Style::new().color(color("bright_blue")),
            status_closed: Style::new().color(color("bright_black")),
            status_pinned: Style::new().color(color("bright_magenta")).bold(),

            priority_critical: Style::new().color(color("bright_red")).bold(),
            priority_high: Style::new().color(color("bright_red")),
            priority_medium: Style::new().color(color("bright_yellow")),
            priority_low: Style::new().color(color("bright_black")),

            border: Style::new().color(color("white")),
            muted: Style::new().color(color("bright_black")),

            ..Self::new()
        }
    }

    /// Create a minimal/monochrome theme.
    ///
    /// Uses only bold/dim styling, no colors.
    #[must_use]
    pub fn minimal() -> Self {
        let dim = Style::new().dim();
        let bold = Style::new().bold();
        let normal = Style::new();

        Self {
            status_open: normal.clone(),
            status_in_progress: bold.clone(),
            status_blocked: bold.clone(),
            status_deferred: dim.clone(),
            status_closed: dim.clone(),
            status_pinned: bold.clone(),

            priority_critical: bold.clone(),
            priority_high: bold.clone(),
            priority_medium: normal.clone(),
            priority_low: dim.clone(),

            type_bug: bold.clone(),
            type_feature: normal.clone(),
            type_task: normal.clone(),
            type_epic: bold.clone(),
            type_docs: normal.clone(),
            type_chore: dim.clone(),

            header: bold.clone(),
            border: dim.clone(),
            muted: dim,
            emphasis: bold,
            success: normal.clone(),
            warning: normal.clone(),
            error: normal.clone(),

            issue_id: normal.clone(),
            label: normal.clone(),
            dependency: normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_theme() {
        let theme = Theme::new();
        // Verify status styles exist
        let _ = theme.status_style(&Status::Open);
        let _ = theme.status_style(&Status::InProgress);
        let _ = theme.status_style(&Status::Blocked);
        let _ = theme.status_style(&Status::Closed);
    }

    #[test]
    fn test_priority_styles() {
        let theme = Theme::new();
        let _ = theme.priority_style(&Priority::CRITICAL);
        let _ = theme.priority_style(&Priority::HIGH);
        let _ = theme.priority_style(&Priority::MEDIUM);
        let _ = theme.priority_style(&Priority::LOW);
    }

    #[test]
    fn test_type_styles() {
        let theme = Theme::new();
        let _ = theme.type_style(&IssueType::Bug);
        let _ = theme.type_style(&IssueType::Feature);
        let _ = theme.type_style(&IssueType::Task);
        let _ = theme.type_style(&IssueType::Epic);
    }

    #[test]
    fn test_dark_theme() {
        let theme = Theme::dark();
        // Should be able to create a dark theme variant
        let _ = theme.status_style(&Status::Open);
    }

    #[test]
    fn test_minimal_theme() {
        let theme = Theme::minimal();
        // Should be able to create a minimal theme variant
        let _ = theme.status_style(&Status::Open);
    }
}
