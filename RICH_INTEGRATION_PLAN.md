# Rich Rust Integration Plan for beads_rust

> A comprehensive, granular plan to integrate rich_rust throughout beads_rust for premium, stylish console output that delights humans without interfering with AI agent workflows.

---

## Executive Summary

**Goal:** Transform `br` from basic colored output to a premium, visually stunning CLI experience using `rich_rust`, while maintaining 100% compatibility with agent/robot modes.

**Key Principle:** Agents using `--json` or `--robot` flags must see zero change. Rich formatting is purely for human observers watching the process.

**Scope:** ~39,636 lines of Rust across 37 commands, all needing thoughtful rich output.

---

## Table of Contents

1. [Architecture Overview](#1-architecture-overview)
2. [Phase 1: Foundation Layer](#2-phase-1-foundation-layer)
3. [Phase 2: Core Components](#3-phase-2-core-components)
4. [Phase 3: Command Integration](#4-phase-3-command-integration)
5. [Phase 4: Advanced Features](#5-phase-4-advanced-features)
6. [Phase 5: Polish & Optimization](#6-phase-5-polish--optimization)
7. [Implementation Guidelines](#7-implementation-guidelines)
8. [Testing Strategy](#8-testing-strategy)
9. [Migration Checklist](#9-migration-checklist)

---

## 1. Architecture Overview

### Current State (beads_rust)

```
User Command → CLI Parser → Command Handler → println!/colored output → stdout
                                    ↓
                              --json flag → serde_json → stdout
```

**Current dependencies:**
- `colored` crate for basic ANSI colors
- Raw `println!` for most output
- `serde_json` for JSON mode

### Target State (with rich_rust)

```
User Command → CLI Parser → Command Handler → OutputContext
                                                   ↓
                                    ┌──────────────┴──────────────┐
                                    ↓                              ↓
                              Human Mode                      Robot Mode
                                    ↓                              ↓
                           RichConsole                     JSON/Plain stdout
                           (Tables, Panels,                (unchanged behavior)
                            Trees, Progress)
```

### Core Design Principles

1. **Zero Agent Impact**: `--json`, `--robot`, `--quiet` bypass all rich formatting
2. **Graceful Degradation**: Auto-detect terminal capabilities, fall back gracefully
3. **Consistent Theming**: Unified color palette and styling across all commands
4. **Performance First**: No rendering overhead when output is piped/redirected
5. **Minimal API Changes**: Existing command logic unchanged, only output layer modified

---

## 2. Phase 1: Foundation Layer

### 2.1 Add rich_rust Dependency

**File:** `Cargo.toml`

```toml
[dependencies]
rich_rust = { version = "0.1", features = ["full"] }

# Remove after migration:
# colored = "3.1"  # DEPRECATED - use rich_rust
```

**Features needed:**
- Core (always): Console, Style, Table, Panel, Rule, Tree, Progress
- `syntax`: For code blocks in issue descriptions
- `markdown`: For rendering markdown in descriptions
- `json`: For pretty-printing JSON in human mode

### 2.2 Create Output Context Module

**File:** `src/output/mod.rs` (NEW)

```rust
//! Output abstraction layer that routes to rich or plain output based on mode.

mod context;
mod theme;
mod components;

pub use context::OutputContext;
pub use theme::Theme;
pub use components::*;
```

**File:** `src/output/context.rs` (NEW)

```rust
use rich_rust::prelude::*;
use crate::cli::GlobalArgs;

/// Central output coordinator that respects robot/json/quiet modes.
pub struct OutputContext {
    /// Rich console for human-readable output
    console: Console,
    /// Theme for consistent styling
    theme: Theme,
    /// Output mode
    mode: OutputMode,
    /// Terminal width (cached)
    width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Full rich formatting (tables, colors, panels)
    Rich,
    /// Plain text, no ANSI codes (for piping)
    Plain,
    /// JSON output only
    Json,
    /// Minimal output (quiet mode)
    Quiet,
}

impl OutputContext {
    /// Create from CLI global args
    pub fn from_args(args: &GlobalArgs) -> Self {
        let mode = Self::detect_mode(args);
        let console = Self::create_console(mode);
        let width = console.width();

        Self {
            console,
            theme: Theme::default(),
            mode,
            width,
        }
    }

    fn detect_mode(args: &GlobalArgs) -> OutputMode {
        // Priority order (highest first):
        // 1. --json flag → Json mode
        // 2. --quiet flag → Quiet mode
        // 3. --no-color flag → Plain mode
        // 4. Not a TTY (piped) → Plain mode
        // 5. Otherwise → Rich mode

        if args.json {
            return OutputMode::Json;
        }
        if args.quiet {
            return OutputMode::Quiet;
        }
        if args.no_color || std::env::var("NO_COLOR").is_ok() {
            return OutputMode::Plain;
        }
        if !is_terminal() {
            return OutputMode::Plain;
        }
        OutputMode::Rich
    }

    fn create_console(mode: OutputMode) -> Console {
        match mode {
            OutputMode::Rich => Console::new(),
            OutputMode::Plain | OutputMode::Quiet => {
                Console::builder()
                    .color_system(None)
                    .force_terminal(false)
                    .build()
            }
            OutputMode::Json => {
                // JSON mode doesn't use console, but create minimal one
                Console::builder()
                    .color_system(None)
                    .force_terminal(false)
                    .build()
            }
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Mode Checks
    // ─────────────────────────────────────────────────────────────

    pub fn is_rich(&self) -> bool { self.mode == OutputMode::Rich }
    pub fn is_json(&self) -> bool { self.mode == OutputMode::Json }
    pub fn is_quiet(&self) -> bool { self.mode == OutputMode::Quiet }
    pub fn is_plain(&self) -> bool { self.mode == OutputMode::Plain }
    pub fn width(&self) -> usize { self.width }

    // ─────────────────────────────────────────────────────────────
    // Output Methods (route based on mode)
    // ─────────────────────────────────────────────────────────────

    /// Print styled text (respects mode)
    pub fn print(&self, content: &str) {
        match self.mode {
            OutputMode::Rich => self.console.print(content),
            OutputMode::Plain => {
                // Strip markup, print plain
                println!("{}", strip_markup(content));
            }
            OutputMode::Quiet => { /* suppress */ }
            OutputMode::Json => { /* JSON output handled separately */ }
        }
    }

    /// Print a renderable component
    pub fn render<R: Renderable>(&self, renderable: &R) {
        if self.is_rich() {
            self.console.print_renderable(renderable);
        }
    }

    /// Print JSON (only in JSON mode)
    pub fn json<T: serde::Serialize>(&self, value: &T) {
        if self.is_json() {
            println!("{}", serde_json::to_string(value).unwrap());
        }
    }

    /// Print JSON pretty (human mode with --json-pretty or similar)
    pub fn json_pretty<T: serde::Serialize>(&self, value: &T) {
        if self.is_rich() {
            let json = rich_rust::renderables::Json::from_value(
                serde_json::to_value(value).unwrap()
            );
            self.console.print_renderable(&json);
        } else if self.is_json() {
            println!("{}", serde_json::to_string_pretty(value).unwrap());
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Semantic Output Methods
    // ─────────────────────────────────────────────────────────────

    /// Success message (green checkmark)
    pub fn success(&self, message: &str) {
        match self.mode {
            OutputMode::Rich => {
                self.console.print(&format!(
                    "[bold green]✓[/] {}",
                    message
                ));
            }
            OutputMode::Plain => println!("✓ {}", message),
            OutputMode::Quiet | OutputMode::Json => {}
        }
    }

    /// Error message (red X, in panel)
    pub fn error(&self, message: &str) {
        match self.mode {
            OutputMode::Rich => {
                let panel = Panel::from_text(message)
                    .title("Error")
                    .border_style(self.theme.error)
                    .title_style(self.theme.error.bold());
                self.console.print_renderable(&panel);
            }
            OutputMode::Plain => eprintln!("Error: {}", message),
            OutputMode::Quiet => eprintln!("Error: {}", message), // Always show errors
            OutputMode::Json => {}
        }
    }

    /// Warning message (yellow)
    pub fn warning(&self, message: &str) {
        match self.mode {
            OutputMode::Rich => {
                self.console.print(&format!(
                    "[bold yellow]⚠[/] [yellow]{}[/]",
                    message
                ));
            }
            OutputMode::Plain => eprintln!("Warning: {}", message),
            OutputMode::Quiet => {}
            OutputMode::Json => {}
        }
    }

    /// Info message (blue)
    pub fn info(&self, message: &str) {
        match self.mode {
            OutputMode::Rich => {
                self.console.print(&format!(
                    "[blue]ℹ[/] {}",
                    message
                ));
            }
            OutputMode::Plain => println!("{}", message),
            OutputMode::Quiet | OutputMode::Json => {}
        }
    }

    /// Section header (rule with title)
    pub fn section(&self, title: &str) {
        if self.is_rich() {
            let rule = Rule::with_title(title)
                .style(self.theme.section);
            self.console.print_renderable(&rule);
        } else if self.is_plain() {
            println!("\n─── {} ───\n", title);
        }
    }

    /// Blank line
    pub fn newline(&self) {
        if !self.is_quiet() && !self.is_json() {
            println!();
        }
    }
}
```

### 2.3 Create Theme Module

**File:** `src/output/theme.rs` (NEW)

```rust
use rich_rust::prelude::*;

/// Consistent color theme for beads_rust CLI.
///
/// Design inspired by premium CLI tools (gh, cargo, rustc).
#[derive(Debug, Clone)]
pub struct Theme {
    // ─────────────────────────────────────────────────────────────
    // Semantic Colors
    // ─────────────────────────────────────────────────────────────

    /// Success: green
    pub success: Style,
    /// Error: red
    pub error: Style,
    /// Warning: yellow
    pub warning: Style,
    /// Info: blue
    pub info: Style,
    /// Dimmed/secondary: gray
    pub dimmed: Style,
    /// Accent: cyan
    pub accent: Style,
    /// Highlight: magenta
    pub highlight: Style,

    // ─────────────────────────────────────────────────────────────
    // Issue-Specific Styles
    // ─────────────────────────────────────────────────────────────

    /// Issue ID (e.g., bd-abc123)
    pub issue_id: Style,
    /// Issue title
    pub issue_title: Style,
    /// Issue description
    pub issue_description: Style,

    // Status colors
    pub status_open: Style,
    pub status_in_progress: Style,
    pub status_blocked: Style,
    pub status_deferred: Style,
    pub status_closed: Style,

    // Priority colors
    pub priority_critical: Style,  // P0
    pub priority_high: Style,      // P1
    pub priority_medium: Style,    // P2
    pub priority_low: Style,       // P3
    pub priority_backlog: Style,   // P4

    // Type colors
    pub type_task: Style,
    pub type_bug: Style,
    pub type_feature: Style,
    pub type_epic: Style,
    pub type_chore: Style,
    pub type_docs: Style,
    pub type_question: Style,

    // ─────────────────────────────────────────────────────────────
    // UI Element Styles
    // ─────────────────────────────────────────────────────────────

    /// Table headers
    pub table_header: Style,
    /// Table borders
    pub table_border: Style,
    /// Panel titles
    pub panel_title: Style,
    /// Panel borders
    pub panel_border: Style,
    /// Section dividers
    pub section: Style,
    /// Labels/tags
    pub label: Style,
    /// Timestamps
    pub timestamp: Style,
    /// Usernames/assignees
    pub username: Style,
    /// Comments
    pub comment: Style,

    // ─────────────────────────────────────────────────────────────
    // Box Style
    // ─────────────────────────────────────────────────────────────

    /// Preferred box style for tables/panels
    pub box_style: &'static BoxChars,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            // Semantic colors
            success: Style::new().green().bold(),
            error: Style::new().red().bold(),
            warning: Style::new().yellow().bold(),
            info: Style::new().blue(),
            dimmed: Style::new().dim(),
            accent: Style::new().cyan(),
            highlight: Style::new().magenta(),

            // Issue ID: cyan, bold (stands out)
            issue_id: Style::new().cyan().bold(),
            issue_title: Style::new().bold(),
            issue_description: Style::new(),

            // Status colors (traffic light metaphor)
            status_open: Style::new().green(),
            status_in_progress: Style::new().blue().bold(),
            status_blocked: Style::new().red(),
            status_deferred: Style::new().yellow().dim(),
            status_closed: Style::new().dim(),

            // Priority colors (heat map: hot = urgent)
            priority_critical: Style::new().red().bold().reverse(),  // P0: RED ALERT
            priority_high: Style::new().red().bold(),                 // P1: red
            priority_medium: Style::new().yellow(),                   // P2: yellow
            priority_low: Style::new().green(),                       // P3: green
            priority_backlog: Style::new().dim(),                     // P4: dim

            // Type colors (semantic associations)
            type_task: Style::new().blue(),
            type_bug: Style::new().red(),
            type_feature: Style::new().green(),
            type_epic: Style::new().magenta().bold(),
            type_chore: Style::new().dim(),
            type_docs: Style::new().cyan(),
            type_question: Style::new().yellow(),

            // UI elements
            table_header: Style::new().bold().underline(),
            table_border: Style::new().dim(),
            panel_title: Style::new().bold(),
            panel_border: Style::new().dim(),
            section: Style::new().cyan().bold(),
            label: Style::new().cyan().dim(),
            timestamp: Style::new().dim(),
            username: Style::new().green(),
            comment: Style::new().italic(),

            // Modern rounded boxes
            box_style: &rich_rust::box_chars::ROUNDED,
        }
    }
}

impl Theme {
    /// Get style for a given status
    pub fn status_style(&self, status: &Status) -> Style {
        match status {
            Status::Open => self.status_open.clone(),
            Status::InProgress => self.status_in_progress.clone(),
            Status::Blocked => self.status_blocked.clone(),
            Status::Deferred => self.status_deferred.clone(),
            Status::Closed => self.status_closed.clone(),
            Status::Tombstone => self.dimmed.clone(),
            Status::Pinned => self.highlight.clone(),
            Status::Custom(_) => self.dimmed.clone(),
        }
    }

    /// Get style for a given priority
    pub fn priority_style(&self, priority: Priority) -> Style {
        match priority.0 {
            0 => self.priority_critical.clone(),
            1 => self.priority_high.clone(),
            2 => self.priority_medium.clone(),
            3 => self.priority_low.clone(),
            _ => self.priority_backlog.clone(),
        }
    }

    /// Get style for a given issue type
    pub fn type_style(&self, issue_type: &IssueType) -> Style {
        match issue_type {
            IssueType::Task => self.type_task.clone(),
            IssueType::Bug => self.type_bug.clone(),
            IssueType::Feature => self.type_feature.clone(),
            IssueType::Epic => self.type_epic.clone(),
            IssueType::Chore => self.type_chore.clone(),
            IssueType::Docs => self.type_docs.clone(),
            IssueType::Question => self.type_question.clone(),
            IssueType::Custom(_) => self.dimmed.clone(),
        }
    }
}
```

---

## 3. Phase 2: Core Components

### 3.1 Issue Table Component

**File:** `src/output/components/issue_table.rs` (NEW)

```rust
use rich_rust::prelude::*;
use crate::model::Issue;
use super::Theme;

/// Renders a list of issues as a beautiful table.
pub struct IssueTable<'a> {
    issues: &'a [Issue],
    theme: &'a Theme,
    columns: IssueTableColumns,
    title: Option<String>,
    show_blocked: bool,
}

#[derive(Default)]
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
    /// Compact: ID, Priority, Type, Title
    pub fn compact() -> Self {
        Self {
            id: true,
            priority: true,
            issue_type: true,
            title: true,
            ..Default::default()
        }
    }

    /// Standard: ID, Priority, Status, Type, Title, Assignee
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

    /// Full: All columns
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
        let mut table = Table::new()
            .box_style(self.theme.box_style)
            .border_style(self.theme.table_border.clone())
            .header_style(self.theme.table_header.clone());

        if let Some(ref title) = self.title {
            table = table.title(title);
        }

        // Add columns based on config
        if self.columns.id {
            table = table.with_column(
                Column::new("ID")
                    .style(self.theme.issue_id.clone())
                    .min_width(10)
            );
        }
        if self.columns.priority {
            table = table.with_column(
                Column::new("P")
                    .justify(JustifyMethod::Center)
                    .width(3)
            );
        }
        if self.columns.status {
            table = table.with_column(
                Column::new("Status")
                    .min_width(8)
            );
        }
        if self.columns.issue_type {
            table = table.with_column(
                Column::new("Type")
                    .min_width(7)
            );
        }
        if self.columns.title {
            table = table.with_column(
                Column::new("Title")
                    .style(self.theme.issue_title.clone())
                    .min_width(20)
                    .max_width(60)
            );
        }
        if self.columns.assignee {
            table = table.with_column(
                Column::new("Assignee")
                    .style(self.theme.username.clone())
                    .max_width(20)
            );
        }
        if self.columns.labels {
            table = table.with_column(
                Column::new("Labels")
                    .style(self.theme.label.clone())
                    .max_width(30)
            );
        }
        if self.columns.created {
            table = table.with_column(
                Column::new("Created")
                    .style(self.theme.timestamp.clone())
                    .width(10)
            );
        }
        if self.columns.updated {
            table = table.with_column(
                Column::new("Updated")
                    .style(self.theme.timestamp.clone())
                    .width(10)
            );
        }

        // Add rows
        for issue in self.issues {
            let mut cells: Vec<String> = vec![];

            if self.columns.id {
                cells.push(issue.id.clone());
            }
            if self.columns.priority {
                cells.push(format!("P{}", issue.priority.0));
            }
            if self.columns.status {
                cells.push(format!("{:?}", issue.status));
            }
            if self.columns.issue_type {
                cells.push(format!("{:?}", issue.issue_type));
            }
            if self.columns.title {
                let mut title = issue.title.clone();
                if title.len() > 57 {
                    title.truncate(57);
                    title.push_str("...");
                }
                cells.push(title);
            }
            if self.columns.assignee {
                cells.push(issue.assignee.clone().unwrap_or_default());
            }
            if self.columns.labels {
                cells.push(issue.labels.join(", "));
            }
            if self.columns.created {
                cells.push(issue.created_at.format("%Y-%m-%d").to_string());
            }
            if self.columns.updated {
                cells.push(issue.updated_at.format("%Y-%m-%d").to_string());
            }

            // Create row with styled cells based on priority/status
            let row = Row::new();
            // TODO: Apply per-cell styling based on priority/status
            table.add_row_cells(cells);
        }

        table
    }
}
```

### 3.2 Issue Detail Panel Component

**File:** `src/output/components/issue_panel.rs` (NEW)

```rust
use rich_rust::prelude::*;
use crate::model::Issue;
use super::Theme;

/// Renders a single issue with full details in a styled panel.
pub struct IssuePanel<'a> {
    issue: &'a Issue,
    theme: &'a Theme,
    show_dependencies: bool,
    show_comments: bool,
    show_history: bool,
}

impl<'a> IssuePanel<'a> {
    pub fn new(issue: &'a Issue, theme: &'a Theme) -> Self {
        Self {
            issue,
            theme,
            show_dependencies: true,
            show_comments: true,
            show_history: false,
        }
    }

    pub fn build(&self) -> Panel<'static> {
        let mut content = Text::new("");

        // Header: ID and Status badges
        content.append(&format!(
            "{}  ",
            self.issue.id
        ), self.theme.issue_id.clone());

        content.append(&format!(
            "[P{}]  ",
            self.issue.priority.0
        ), self.theme.priority_style(self.issue.priority));

        content.append(&format!(
            "{:?}  ",
            self.issue.status
        ), self.theme.status_style(&self.issue.status));

        content.append(&format!(
            "{:?}\n\n",
            self.issue.issue_type
        ), self.theme.type_style(&self.issue.issue_type));

        // Title
        content.append(&self.issue.title, self.theme.issue_title.clone());
        content.append("\n", Style::new());

        // Description
        if let Some(ref desc) = self.issue.description {
            content.append("\n", Style::new());
            content.append(desc, self.theme.issue_description.clone());
            content.append("\n", Style::new());
        }

        // Metadata section
        content.append("\n───────────────────────────────────\n", self.theme.dimmed.clone());

        // Assignee
        if let Some(ref assignee) = self.issue.assignee {
            content.append("Assignee: ", self.theme.dimmed.clone());
            content.append(&format!("{}\n", assignee), self.theme.username.clone());
        }

        // Labels
        if !self.issue.labels.is_empty() {
            content.append("Labels:   ", self.theme.dimmed.clone());
            for (i, label) in self.issue.labels.iter().enumerate() {
                if i > 0 {
                    content.append(", ", self.theme.dimmed.clone());
                }
                content.append(label, self.theme.label.clone());
            }
            content.append("\n", Style::new());
        }

        // Timestamps
        content.append("Created:  ", self.theme.dimmed.clone());
        content.append(
            &format!("{}\n", self.issue.created_at.format("%Y-%m-%d %H:%M")),
            self.theme.timestamp.clone()
        );

        content.append("Updated:  ", self.theme.dimmed.clone());
        content.append(
            &format!("{}\n", self.issue.updated_at.format("%Y-%m-%d %H:%M")),
            self.theme.timestamp.clone()
        );

        // Dependencies
        if self.show_dependencies && !self.issue.dependencies.is_empty() {
            content.append("\n───────────────────────────────────\n", self.theme.dimmed.clone());
            content.append("Dependencies:\n", self.theme.dimmed.bold());
            for dep in &self.issue.dependencies {
                content.append(&format!("  → {} ", dep.depends_on_id), self.theme.issue_id.clone());
                content.append(&format!("({:?})\n", dep.dep_type), self.theme.dimmed.clone());
            }
        }

        // Build panel
        let segments = content.render("");

        Panel::new(segments)
            .title(&self.issue.id)
            .box_style(self.theme.box_style)
            .border_style(self.theme.panel_border.clone())
            .title_style(self.theme.issue_id.clone())
    }
}
```

### 3.3 Dependency Tree Component

**File:** `src/output/components/dep_tree.rs` (NEW)

```rust
use rich_rust::prelude::*;
use crate::model::{Issue, Dependency};
use super::Theme;

/// Renders a dependency tree for an issue.
pub struct DependencyTree<'a> {
    root_issue: &'a Issue,
    all_issues: &'a [Issue],
    theme: &'a Theme,
    max_depth: usize,
}

impl<'a> DependencyTree<'a> {
    pub fn new(root: &'a Issue, all: &'a [Issue], theme: &'a Theme) -> Self {
        Self {
            root_issue: root,
            all_issues: all,
            theme,
            max_depth: 10,
        }
    }

    pub fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    pub fn build(&self) -> Tree {
        let root_node = self.build_node(self.root_issue, 0);

        Tree::new(root_node)
            .guides(TreeGuides::Rounded)
            .style(self.theme.dimmed.clone())
    }

    fn build_node(&self, issue: &Issue, depth: usize) -> TreeNode {
        // Create label with ID, status, and title
        let label = format!(
            "{} [{}] {}",
            issue.id,
            format!("{:?}", issue.status).chars().next().unwrap_or('?'),
            truncate(&issue.title, 40)
        );

        let mut node = TreeNode::new(&label);

        // Recursively add dependencies (if not too deep)
        if depth < self.max_depth {
            for dep in &issue.dependencies {
                if let Some(dep_issue) = self.find_issue(&dep.depends_on_id) {
                    let child = self.build_node(dep_issue, depth + 1);
                    node = node.child(child);
                } else {
                    // Dependency not found (external or deleted)
                    let missing = TreeNode::new(&format!(
                        "{} [?] (not found)",
                        dep.depends_on_id
                    ));
                    node = node.child(missing);
                }
            }
        }

        node
    }

    fn find_issue(&self, id: &str) -> Option<&Issue> {
        self.all_issues.iter().find(|i| i.id == id)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}
```

### 3.4 Progress Indicator Component

**File:** `src/output/components/progress.rs` (NEW)

```rust
use rich_rust::prelude::*;
use std::io::{self, Write};
use super::Theme;

/// Progress tracker for long operations (sync, import, export).
pub struct ProgressTracker<'a> {
    theme: &'a Theme,
    total: usize,
    current: usize,
    description: String,
    bar: ProgressBar,
}

impl<'a> ProgressTracker<'a> {
    pub fn new(theme: &'a Theme, total: usize, description: impl Into<String>) -> Self {
        let bar = ProgressBar::new()
            .total(total)
            .width(40)
            .bar_style(BarStyle::Block)
            .completed_style(theme.accent.clone())
            .remaining_style(theme.dimmed.clone());

        Self {
            theme,
            total,
            current: 0,
            description: description.into(),
            bar,
        }
    }

    pub fn tick(&mut self) {
        self.current += 1;
        self.bar.set_progress(self.current);
    }

    pub fn set(&mut self, current: usize) {
        self.current = current;
        self.bar.set_progress(current);
    }

    pub fn render(&self, console: &Console) {
        // Clear line and render progress
        print!("\r");
        console.print(&format!(
            "[bold]{}[/]: ",
            self.description
        ));
        console.print_renderable(&self.bar);
        print!(" {}/{}", self.current, self.total);
        io::stdout().flush().ok();
    }

    pub fn finish(&self, console: &Console) {
        println!();
        console.print(&format!(
            "[bold green]✓[/] {} complete ({} items)",
            self.description,
            self.total
        ));
    }
}
```

### 3.5 Stats Panel Component

**File:** `src/output/components/stats.rs` (NEW)

```rust
use rich_rust::prelude::*;
use super::Theme;

/// Renders statistics as a formatted panel with counts.
pub struct StatsPanel<'a> {
    title: String,
    stats: Vec<(&'a str, usize, Style)>,
    theme: &'a Theme,
}

impl<'a> StatsPanel<'a> {
    pub fn new(title: impl Into<String>, theme: &'a Theme) -> Self {
        Self {
            title: title.into(),
            stats: vec![],
            theme,
        }
    }

    pub fn add(&mut self, label: &'a str, count: usize, style: Style) -> &mut Self {
        self.stats.push((label, count, style));
        self
    }

    pub fn build(&self) -> Panel<'static> {
        let mut table = Table::new()
            .box_style(&rich_rust::box_chars::MINIMAL)
            .show_header(false);

        table = table
            .with_column(Column::new("Label").min_width(15))
            .with_column(Column::new("Count").justify(JustifyMethod::Right).min_width(6));

        for (label, count, _style) in &self.stats {
            table.add_row_cells([*label, &count.to_string()]);
        }

        Panel::from_renderable(&table)
            .title(&self.title)
            .box_style(self.theme.box_style)
            .border_style(self.theme.panel_border.clone())
            .title_style(self.theme.panel_title.clone())
    }
}
```

---

## 4. Phase 3: Command Integration

### 4.1 Command Output Mapping

| Command | Current Output | Rich Output |
|---------|----------------|-------------|
| `init` | "Initialized beads workspace..." | Success panel with path |
| `create` | "Created: bd-abc123" | Success message + issue summary |
| `list` | Plain table | Rich table with colored status/priority |
| `show` | Key-value pairs | Detailed issue panel |
| `ready` | Plain table | Highlighted "ready" table with tips |
| `blocked` | Plain table | Table + blocking chain tree |
| `close` | "Closed: bd-abc123" | Success message with summary |
| `update` | "Updated: bd-abc123" | Success + changed fields highlight |
| `search` | Plain results | Results table with match highlighting |
| `sync` | Progress dots | Progress bar with stats |
| `stats` | Plain counts | Stats panel with bars |
| `dep tree` | ASCII tree | Rich tree with status colors |
| `doctor` | Plain diagnostics | Diagnostic panels |
| `config` | Plain list | Config table |
| `audit` | Plain events | Event timeline |
| `stale` | Plain list | Table with staleness indicators |

### 4.2 Per-Command Integration Pattern

Each command handler needs modification:

```rust
// BEFORE (current pattern):
pub fn run_list(args: ListArgs, storage: &SqliteStorage) -> Result<()> {
    let issues = storage.list_issues(&filters)?;

    if args.json {
        println!("{}", serde_json::to_string(&issues)?);
    } else {
        for issue in &issues {
            println!("{}\t{}\t{}", issue.id, issue.priority, issue.title);
        }
    }

    Ok(())
}

// AFTER (with rich_rust):
pub fn run_list(args: ListArgs, storage: &SqliteStorage, ctx: &OutputContext) -> Result<()> {
    let issues = storage.list_issues(&filters)?;

    // JSON mode: unchanged behavior
    if ctx.is_json() {
        ctx.json(&issues);
        return Ok(());
    }

    // Quiet mode: minimal output
    if ctx.is_quiet() {
        for issue in &issues {
            println!("{}", issue.id);
        }
        return Ok(());
    }

    // Rich/Plain mode: beautiful table
    if issues.is_empty() {
        ctx.info("No issues found matching filters.");
        return Ok(());
    }

    let table = IssueTable::new(&issues, ctx.theme())
        .title(format!("{} issues", issues.len()))
        .columns(IssueTableColumns::standard())
        .build();

    ctx.render(&table);

    // Show summary
    ctx.newline();
    ctx.info(&format!(
        "Showing {} of {} total issues",
        issues.len(),
        storage.count_issues()?
    ));

    Ok(())
}
```

### 4.3 Command-Specific Rich Output Designs

#### `br init`

```
╭────────────────────────────────────────────────────╮
│  ✓ Initialized beads workspace                     │
│                                                    │
│  Location: /path/to/project/.beads                 │
│  Database: beads.db                                │
│  Export:   issues.jsonl                            │
│                                                    │
│  Next steps:                                       │
│    br create "Your first issue" --type task        │
│    br list                                         │
╰────────────────────────────────────────────────────╯
```

#### `br list`

```
────────────────────── 12 issues ──────────────────────
╭──────────┬───┬────────────┬─────────┬──────────────────────────────────────┬──────────────╮
│ ID       │ P │ Status     │ Type    │ Title                                │ Assignee     │
├──────────┼───┼────────────┼─────────┼──────────────────────────────────────┼──────────────┤
│ bd-a1b2c3│ 0 │ InProgress │ Bug     │ Fix critical login timeout           │ alice@co     │
│ bd-d4e5f6│ 1 │ Open       │ Feature │ Add OAuth2 support                   │              │
│ bd-g7h8i9│ 2 │ Blocked    │ Task    │ Update documentation                 │ bob@co       │
│ ...      │   │            │         │                                      │              │
╰──────────┴───┴────────────┴─────────┴──────────────────────────────────────┴──────────────╯

ℹ Showing 12 of 47 total issues
```

#### `br show bd-abc123`

```
╭─────────────────────────── bd-a1b2c3 ────────────────────────────╮
│                                                                   │
│  bd-a1b2c3  [P0]  InProgress  Bug                                 │
│                                                                   │
│  Fix critical login timeout                                       │
│                                                                   │
│  Users report login times out after 30 seconds on slow            │
│  connections. Need to increase timeout and add retry logic.       │
│                                                                   │
│  ─────────────────────────────────────────────────────────────    │
│  Assignee: alice@example.com                                      │
│  Labels:   backend, auth, urgent                                  │
│  Created:  2024-01-15 14:30                                       │
│  Updated:  2024-01-16 09:15                                       │
│                                                                   │
│  ─────────────────────────────────────────────────────────────    │
│  Dependencies:                                                    │
│    → bd-xyz789 (Blocks) - Database connection pooling             │
│                                                                   │
╰───────────────────────────────────────────────────────────────────╯
```

#### `br ready`

```
────────────────────── Ready to Work ──────────────────────

  These issues have no blockers and are ready for action:

╭──────────┬───┬─────────┬────────────────────────────────────────╮
│ ID       │ P │ Type    │ Title                                  │
├──────────┼───┼─────────┼────────────────────────────────────────┤
│ bd-d4e5f6│ 1 │ Feature │ Add OAuth2 support                     │
│ bd-j1k2l3│ 2 │ Task    │ Refactor storage layer                 │
│ bd-m4n5o6│ 3 │ Docs    │ Update API documentation               │
╰──────────┴───┴─────────┴────────────────────────────────────────╯

💡 Tip: Claim work with: br update bd-d4e5f6 --status in_progress
```

#### `br sync --flush-only`

```
────────────────────── Syncing ──────────────────────

Exporting issues to JSONL...

  ████████████████████████████████████████ 47/47

✓ Export complete

  Issues exported: 47
  Labels:          23
  Dependencies:    15
  Comments:        89

  Output: .beads/issues.jsonl (24.5 KB)

💡 Next: git add .beads/ && git commit -m "Sync issues"
```

#### `br dep tree bd-abc123`

```
────────────────────── Dependency Tree ──────────────────────

bd-a1b2c3 [I] Fix critical login timeout
├── bd-xyz789 [O] Database connection pooling
│   └── bd-qrs012 [C] Set up database infrastructure
└── bd-tuv345 [O] Add retry mechanism to HTTP client

Legend: [O]=Open [I]=InProgress [B]=Blocked [C]=Closed
```

#### `br stats`

```
╭───────────────────── Project Statistics ─────────────────────╮
│                                                               │
│  By Status                        By Priority                 │
│  ──────────────                   ────────────                │
│  Open:        23  ████████        P0 Critical:  2  █         │
│  InProgress:   5  ██              P1 High:      8  ███       │
│  Blocked:      4  █               P2 Medium:   15  ██████    │
│  Deferred:     3  █               P3 Low:      12  █████     │
│  Closed:      12  ████            P4 Backlog:  10  ████      │
│                                                               │
│  By Type                                                      │
│  ───────                                                      │
│  Task:     18  ███████                                        │
│  Bug:      12  █████                                          │
│  Feature:   8  ███                                            │
│  Epic:      3  █                                              │
│  Other:     6  ██                                             │
│                                                               │
│  Total Issues: 47                                             │
│  Avg Age: 12 days                                             │
│  Blocked Rate: 8.5%                                           │
│                                                               │
╰───────────────────────────────────────────────────────────────╯
```

---

## 5. Phase 4: Advanced Features

### 5.1 Syntax Highlighting for Code in Descriptions

When issue descriptions contain code blocks (markdown fenced blocks), render with syntax highlighting:

```rust
// In IssuePanel, detect code blocks and render with Syntax
if self.issue.description.contains("```") {
    // Parse markdown, extract code blocks
    // Render with Syntax component
    let syntax = Syntax::new(&code, &language)
        .line_numbers(false)
        .theme("base16-ocean.dark");
    // Embed in panel
}
```

### 5.2 Markdown Rendering for Rich Descriptions

Use rich_rust's Markdown renderer for description fields:

```rust
if args.render_markdown {
    let md = Markdown::new(&issue.description.unwrap_or_default());
    ctx.render(&md);
}
```

### 5.3 Interactive-Style Progress for Long Operations

For operations like `sync --import-only` with many issues:

```rust
let mut progress = ProgressTracker::new(ctx.theme(), total_issues, "Importing");

for (i, issue) in issues.iter().enumerate() {
    storage.upsert_issue(issue)?;
    progress.tick();

    // Render every 10 items to reduce flicker
    if i % 10 == 0 {
        progress.render(&ctx.console);
    }
}

progress.finish(&ctx.console);
```

### 5.4 Error Panels with Context

Wrap errors in informative panels:

```rust
ctx.error_panel(
    "Issue Not Found",
    &format!("Could not find issue with ID: {}", id),
    &[
        "Check the ID is correct",
        "Run `br list` to see available issues",
        "The issue may have been deleted",
    ]
);
```

### 5.5 Diff Display for Updates

When updating issues, show what changed:

```rust
ctx.print("[dim]Changes:[/]");
let mut diff_table = Table::new()
    .box_style(&MINIMAL)
    .with_column(Column::new("Field"))
    .with_column(Column::new("Old"))
    .with_column(Column::new("New"));

if old.priority != new.priority {
    diff_table.add_row_cells(["priority", &old.priority.to_string(), &new.priority.to_string()]);
}
// etc.

ctx.render(&diff_table);
```

---

## 6. Phase 5: Polish & Optimization

### 6.1 Performance Optimizations

1. **Lazy Console Creation**: Don't create Console if `--json` mode
2. **Width Caching**: Cache terminal width, don't re-query per command
3. **Style Caching**: Theme styles are static, compute once
4. **Batch Rendering**: Collect all segments, write once

### 6.2 Accessibility Considerations

1. **ASCII Fallback**: Detect `TERM=dumb` or `--ascii` flag for simple output
2. **High Contrast**: Respect `COLORTERM` and `NO_COLOR` env vars
3. **Screen Reader**: Ensure plain text is meaningful without ANSI

### 6.3 Testing Rich Output

1. **Snapshot Tests**: Capture rendered output, compare to golden files
2. **Mode Testing**: Test each output mode (Rich, Plain, JSON, Quiet)
3. **Terminal Emulation**: Test with different TERM values

### 6.4 Documentation

1. **Update README**: Show rich output screenshots
2. **AGENTS.md**: Document that `--json` mode is unchanged
3. **Examples**: Add output examples to help text

---

## 7. Implementation Guidelines

### 7.1 Do's

- ✅ Always check `ctx.is_json()` before any rich output
- ✅ Use theme colors consistently (don't hardcode colors)
- ✅ Provide plain-text fallbacks for all components
- ✅ Test with `NO_COLOR=1` to verify degradation
- ✅ Keep JSON output byte-identical to current behavior
- ✅ Use semantic output methods (`ctx.success()`, `ctx.error()`)

### 7.2 Don'ts

- ❌ Don't use `println!` directly in command handlers
- ❌ Don't assume terminal supports colors
- ❌ Don't break existing `--json` output format
- ❌ Don't add mandatory interactive elements
- ❌ Don't use animations or live updates (agents can't handle)
- ❌ Don't make output width-dependent in JSON mode

### 7.3 Migration Pattern

For each command:

1. Add `OutputContext` parameter to handler
2. Move JSON output to `ctx.json()` call
3. Replace `println!` with `ctx.print()` or semantic methods
4. Replace tables with `IssueTable` or rich_rust Table
5. Add success/error messages with `ctx.success()`/`ctx.error()`
6. Test all four modes: Rich, Plain, JSON, Quiet

---

## 8. Testing Strategy

### 8.1 Unit Tests

```rust
#[test]
fn test_output_mode_detection() {
    let args = GlobalArgs { json: true, ..Default::default() };
    let ctx = OutputContext::from_args(&args);
    assert!(ctx.is_json());
    assert!(!ctx.is_rich());
}

#[test]
fn test_theme_priority_colors() {
    let theme = Theme::default();
    assert!(theme.priority_style(Priority(0)).attributes.contains(BOLD));
}
```

### 8.2 Integration Tests

```rust
#[test]
fn test_list_json_unchanged() {
    let output = run_command(&["br", "list", "--json"]);
    let parsed: Vec<Issue> = serde_json::from_str(&output).unwrap();
    // Verify structure
}

#[test]
fn test_list_rich_has_table() {
    let output = run_command(&["br", "list"]);
    assert!(output.contains("╭")); // Table border
}
```

### 8.3 Snapshot Tests

```rust
#[test]
fn test_show_output_snapshot() {
    let output = run_command(&["br", "show", "bd-test123"]);
    insta::assert_snapshot!(output);
}
```

### 8.4 Mode Matrix Testing

| Command | `--json` | `--quiet` | `--no-color` | Default |
|---------|----------|-----------|--------------|---------|
| list | ✓ JSON array | ✓ IDs only | ✓ Plain table | ✓ Rich table |
| show | ✓ JSON object | ✓ Nothing | ✓ Plain text | ✓ Rich panel |
| create | ✓ JSON object | ✓ ID only | ✓ Plain msg | ✓ Success msg |
| sync | ✓ JSON stats | ✓ Nothing | ✓ Plain progress | ✓ Rich progress |

---

## 9. Migration Checklist

### Phase 1: Foundation (Week 1)
- [ ] Add rich_rust dependency to Cargo.toml
- [ ] Create `src/output/mod.rs` module structure
- [ ] Implement `OutputContext` with mode detection
- [ ] Implement `Theme` with all semantic colors
- [ ] Add `OutputContext` to command context

### Phase 2: Core Components (Week 2)
- [ ] Implement `IssueTable` component
- [ ] Implement `IssuePanel` component
- [ ] Implement `DependencyTree` component
- [ ] Implement `ProgressTracker` component
- [ ] Implement `StatsPanel` component

### Phase 3: High-Traffic Commands (Week 3)
- [ ] Migrate `list` command
- [ ] Migrate `show` command
- [ ] Migrate `ready` command
- [ ] Migrate `create` command
- [ ] Migrate `close` command
- [ ] Migrate `update` command

### Phase 4: Medium-Traffic Commands (Week 4)
- [ ] Migrate `search` command
- [ ] Migrate `sync` command
- [ ] Migrate `dep` subcommands
- [ ] Migrate `label` subcommands
- [ ] Migrate `blocked` command
- [ ] Migrate `stale` command

### Phase 5: Low-Traffic Commands (Week 5)
- [ ] Migrate `init` command
- [ ] Migrate `stats` command
- [ ] Migrate `doctor` command
- [ ] Migrate `config` command
- [ ] Migrate `audit` command
- [ ] Migrate remaining commands

### Phase 6: Polish (Week 6)
- [ ] Add syntax highlighting for code blocks
- [ ] Add markdown rendering option
- [ ] Performance optimization
- [ ] Comprehensive testing
- [ ] Update documentation
- [ ] Remove `colored` dependency

---

## Appendix A: File Inventory

Files to create:
- `src/output/mod.rs`
- `src/output/context.rs`
- `src/output/theme.rs`
- `src/output/components/mod.rs`
- `src/output/components/issue_table.rs`
- `src/output/components/issue_panel.rs`
- `src/output/components/dep_tree.rs`
- `src/output/components/progress.rs`
- `src/output/components/stats.rs`

Files to modify:
- `Cargo.toml` (add rich_rust, eventually remove colored)
- `src/lib.rs` (add output module)
- `src/cli/mod.rs` (add OutputContext to command dispatch)
- `src/cli/commands/*.rs` (all 37 command files)

---

## Appendix B: Theme Color Reference

```
Success:     #50fa7b (green)
Error:       #ff5555 (red)
Warning:     #f1fa8c (yellow)
Info:        #8be9fd (cyan)
Accent:      #bd93f9 (purple)
Dimmed:      #6272a4 (gray)

Priority 0:  #ff5555 reverse (CRITICAL)
Priority 1:  #ff5555 (red)
Priority 2:  #f1fa8c (yellow)
Priority 3:  #50fa7b (green)
Priority 4:  #6272a4 (dim)

Status Open:        #50fa7b
Status InProgress:  #8be9fd bold
Status Blocked:     #ff5555
Status Deferred:    #f1fa8c dim
Status Closed:      #6272a4
```

---

## Appendix C: Example Session (Before/After)

### Before (current output)

```
$ br ready
bd-d4e5f6  1  Feature  Add OAuth2 support
bd-j1k2l3  2  Task     Refactor storage layer

$ br show bd-d4e5f6
ID: bd-d4e5f6
Title: Add OAuth2 support
Status: Open
Priority: 1
Type: Feature
Assignee:
Created: 2024-01-15T14:30:00Z
```

### After (with rich_rust)

```
$ br ready
────────────────────── Ready to Work ──────────────────────

╭──────────┬───┬─────────┬────────────────────────────────────────╮
│ ID       │ P │ Type    │ Title                                  │
├──────────┼───┼─────────┼────────────────────────────────────────┤
│ bd-d4e5f6│ 1 │ Feature │ Add OAuth2 support                     │
│ bd-j1k2l3│ 2 │ Task    │ Refactor storage layer                 │
╰──────────┴───┴─────────┴────────────────────────────────────────╯

💡 Claim with: br update <id> --status in_progress

$ br show bd-d4e5f6
╭─────────────────────────── bd-d4e5f6 ────────────────────────────╮
│                                                                   │
│  bd-d4e5f6  [P1]  Open  Feature                                   │
│                                                                   │
│  Add OAuth2 support                                               │
│                                                                   │
│  ─────────────────────────────────────────────────────────────    │
│  Created:  2024-01-15 14:30                                       │
│  Updated:  2024-01-15 14:30                                       │
│                                                                   │
╰───────────────────────────────────────────────────────────────────╯
```

---

*End of Integration Plan*
