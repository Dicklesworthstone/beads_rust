//! Output formatting for `beads_rust`.
//!
//! Supports human-readable text output, machine-parseable JSON, and CSV export.
//! Robot mode sends clean JSON to stdout with diagnostics to stderr.
//!
//! # Output Types
//!
//! These types match the classic bd JSON schemas for CLI compatibility:
//! - [`IssueWithCounts`] - Issue with dependency/dependent counts (list/search)
//! - [`IssueDetails`] - Issue with full relations (show)
//! - [`BlockedIssue`] - Issue with blocking info (blocked)
//! - [`TreeNode`] - Issue in dependency tree (dep tree)
//! - [`Statistics`] - Aggregate stats (stats/status)
//!
//! # CSV Output
//!
//! The [`csv`] module provides CSV formatting with:
//! - Configurable field selection via `--fields`
//! - Proper escaping of commas, quotes, and newlines
//!
//! # Rich Output
//!
//! The [`rich`] module provides enhanced terminal output using `rich_rust`:
//! - Tables with styled columns for issue lists
//! - Panels for detailed issue views
//! - Trees for dependency visualization
//! - Consistent theming via [`Theme`]
//!
//! Output mode is determined by [`OutputContext`]:
//! - Rich: TTY with colors enabled
//! - Plain: TTY with `--no-color` or not a TTY
//! - JSON: `--json` flag
//! - Quiet: `--quiet` flag

pub mod context;
pub mod csv;
mod output;
pub mod rich;
mod text;
pub mod theme;

pub use output::{
    BlockedIssue, BlockedIssueOutput, Breakdown, BreakdownEntry, IssueDetails, IssueWithCounts,
    IssueWithDependencyMetadata, ReadyIssue, RecentActivity, StaleIssue, Statistics, StatsSummary,
    TreeNode,
};
pub use text::{
    TextFormatOptions, format_issue_line, format_issue_line_with, format_priority,
    format_priority_badge, format_priority_label, format_status_icon, format_status_icon_colored,
    format_status_label, format_type_badge, format_type_badge_colored, terminal_width,
    truncate_title,
};

// Rich output support
pub use context::{OutputContext, OutputMode};
pub use theme::Theme;
