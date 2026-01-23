//! AGENTS.md blurb detection and management.
//!
//! This module provides functionality to detect, add, update, and remove
//! beads workflow instructions in AGENTS.md or CLAUDE.md files.

use crate::error::{BeadsError, Result};
use crate::output::{OutputContext, OutputMode};
use regex::Regex;
use rich_rust::prelude::*;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Current version of the agent instructions blurb.
/// Increment this when making breaking changes to the blurb format.
pub const BLURB_VERSION: u8 = 1;

/// Start marker for the blurb (includes version).
pub const BLURB_START_MARKER: &str = "<!-- br-agent-instructions-v1 -->";

/// End marker for the blurb.
pub const BLURB_END_MARKER: &str = "<!-- end-br-agent-instructions -->";

/// Supported agent file names in order of preference.
pub const SUPPORTED_AGENT_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", "agents.md", "claude.md"];

/// The agent instructions blurb to append to AGENTS.md files.
pub const AGENT_BLURB: &str = r#"<!-- br-agent-instructions-v1 -->

---

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`/`bd`) for issue tracking. Issues are stored in `.beads/` and tracked in git.

### Essential Commands

```bash
# View ready issues (unblocked, not deferred)
br ready              # or: bd ready

# List and search
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br search "keyword"   # Full-text search

# Create and update
br create --title="..." --description="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason="Completed"
br close <id1> <id2>  # Close multiple issues at once

# Sync with git
br sync --flush-only  # Export DB to JSONL
br sync --status      # Check sync status
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Always run `br sync --flush-only` at session end

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers 0-4, not words)
- **Types**: task, bug, feature, epic, chore, docs, question
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads changes to JSONL
git commit -m "..."     # Commit everything
git push                # Push to remote
```

### Best Practices

- Check `br ready` at session start to find available work
- Update status as you work (in_progress → closed)
- Create new issues with `br create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always sync before ending session

<!-- end-br-agent-instructions -->"#;

/// Result of detecting an agent config file.
#[derive(Debug, Clone, Default)]
pub struct AgentFileDetection {
    /// Full path to the found file (None if not found).
    pub file_path: Option<PathBuf>,
    /// Type of file found ("AGENTS.md", "CLAUDE.md", etc.).
    pub file_type: Option<String>,
    /// Whether the file contains our blurb (current or legacy).
    pub has_blurb: bool,
    /// Whether the file has the legacy (bv) blurb format.
    pub has_legacy_blurb: bool,
    /// Version of the blurb found (0 if none or legacy).
    pub blurb_version: u8,
    /// File content (if read).
    pub content: Option<String>,
}

impl AgentFileDetection {
    /// Returns true if an agent file was detected.
    #[must_use]
    pub const fn found(&self) -> bool {
        self.file_path.is_some()
    }

    /// Returns true if the file exists but doesn't have our blurb.
    #[must_use]
    pub const fn needs_blurb(&self) -> bool {
        self.found() && !self.has_blurb
    }

    /// Returns true if the file has an older version that needs upgrade.
    #[must_use]
    pub const fn needs_upgrade(&self) -> bool {
        if self.has_legacy_blurb {
            return true;
        }
        self.has_blurb && self.blurb_version < BLURB_VERSION
    }
}

/// Check if content contains the br agent blurb.
#[must_use]
pub fn contains_blurb(content: &str) -> bool {
    content.contains("<!-- br-agent-instructions-v")
}

/// Check if content contains the legacy bv blurb.
#[must_use]
pub fn contains_legacy_blurb(content: &str) -> bool {
    // Check for bv blurb markers
    content.contains("<!-- bv-agent-instructions-v")
}

/// Check if content contains any blurb (br or bv).
#[must_use]
pub fn contains_any_blurb(content: &str) -> bool {
    contains_blurb(content) || contains_legacy_blurb(content)
}

/// Extract the version number from an existing blurb.
#[must_use]
#[allow(clippy::missing_panics_doc)] // Regex is static and valid
pub fn get_blurb_version(content: &str) -> u8 {
    let re = Regex::new(r"<!-- br-agent-instructions-v(\d+) -->").unwrap();
    if let Some(caps) = re.captures(content) {
        if let Some(m) = caps.get(1) {
            return m.as_str().parse().unwrap_or(0);
        }
    }
    0
}

/// Detect an agent file in the given directory.
#[must_use]
pub fn detect_agent_file(work_dir: &Path) -> AgentFileDetection {
    // Try uppercase variants first (preferred)
    for filename in SUPPORTED_AGENT_FILES
        .iter()
        .filter(|f| f.starts_with(|c: char| c.is_uppercase()))
    {
        let file_path = work_dir.join(filename);
        if let Some(detection) = check_agent_file(&file_path, filename) {
            return detection;
        }
    }

    // Try lowercase variants as fallback
    for filename in SUPPORTED_AGENT_FILES
        .iter()
        .filter(|f| f.starts_with(|c: char| c.is_lowercase()))
    {
        let file_path = work_dir.join(filename);
        if let Some(detection) = check_agent_file(&file_path, filename) {
            return detection;
        }
    }

    AgentFileDetection::default()
}

/// Check a specific file path for agent configuration.
fn check_agent_file(file_path: &Path, file_type: &str) -> Option<AgentFileDetection> {
    if !file_path.exists() || file_path.is_dir() {
        return None;
    }

    let Ok(content) = fs::read_to_string(file_path) else {
        // File exists but not readable
        return Some(AgentFileDetection {
            file_path: Some(file_path.to_path_buf()),
            file_type: Some(file_type.to_string()),
            ..Default::default()
        });
    };

    let has_legacy = contains_legacy_blurb(&content);
    let has_br_blurb = contains_blurb(&content);

    Some(AgentFileDetection {
        file_path: Some(file_path.to_path_buf()),
        file_type: Some(file_type.to_string()),
        has_blurb: has_br_blurb || has_legacy,
        has_legacy_blurb: has_legacy,
        blurb_version: get_blurb_version(&content),
        content: Some(content),
    })
}

/// Detect an agent file, searching parent directories.
#[must_use]
pub fn detect_agent_file_in_parents(work_dir: &Path, max_levels: usize) -> AgentFileDetection {
    let mut current_dir = work_dir.to_path_buf();

    for _ in 0..=max_levels {
        let detection = detect_agent_file(&current_dir);
        if detection.found() {
            return detection;
        }

        // Move to parent
        match current_dir.parent() {
            Some(parent) if parent != current_dir => {
                current_dir = parent.to_path_buf();
            }
            _ => break, // Reached root
        }
    }

    AgentFileDetection::default()
}

/// Append the blurb to content.
#[must_use]
pub fn append_blurb(content: &str) -> String {
    let mut result = content.to_string();
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result.push('\n');
    result.push_str(AGENT_BLURB);
    result.push('\n');
    result
}

/// Remove an existing br blurb from content.
#[must_use]
pub fn remove_blurb(content: &str) -> String {
    let start_marker = "<!-- br-agent-instructions-v";
    let Some(start_idx) = content.find(start_marker) else {
        return content.to_string();
    };

    let Some(end_pos) = content.find(BLURB_END_MARKER) else {
        return content.to_string();
    };
    let end_idx = end_pos + BLURB_END_MARKER.len();

    // Trim whitespace around the removed section
    let mut start = start_idx;
    let mut end = end_idx;

    // Remove trailing newlines
    while end < content.len() && content[end..].starts_with('\n') {
        end += 1;
    }

    // Remove leading newlines (up to 2)
    let mut removed_leading = 0;
    while start > 0 && content[..start].ends_with('\n') && removed_leading < 2 {
        start -= 1;
        removed_leading += 1;
    }

    format!("{}{}", &content[..start], &content[end..])
}

/// Remove legacy bv blurb from content.
#[must_use]
pub fn remove_legacy_blurb(content: &str) -> String {
    if !contains_legacy_blurb(content) {
        return content.to_string();
    }

    let start_marker = "<!-- bv-agent-instructions-v";
    let end_marker = "<!-- end-bv-agent-instructions -->";

    let Some(start_idx) = content.find(start_marker) else {
        return content.to_string();
    };

    let Some(end_pos) = content.find(end_marker) else {
        return content.to_string();
    };
    let end_idx = end_pos + end_marker.len();

    // Trim whitespace around the removed section
    let mut start = start_idx;
    let mut end = end_idx;

    while end < content.len() && content[end..].starts_with('\n') {
        end += 1;
    }

    let mut removed_leading = 0;
    while start > 0 && content[..start].ends_with('\n') && removed_leading < 2 {
        start -= 1;
        removed_leading += 1;
    }

    format!("{}{}", &content[..start], &content[end..])
}

/// Update an existing blurb to the current version.
#[must_use]
pub fn update_blurb(content: &str) -> String {
    let content = remove_legacy_blurb(content);
    let content = remove_blurb(&content);
    append_blurb(&content)
}

/// Get the preferred path for a new agent file.
#[must_use]
pub fn get_preferred_agent_file_path(work_dir: &Path) -> PathBuf {
    work_dir.join("AGENTS.md")
}

/// Arguments for the agents command.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct AgentsArgs {
    /// Add blurb to AGENTS.md (creates file if needed).
    pub add: bool,
    /// Remove blurb from AGENTS.md.
    pub remove: bool,
    /// Update blurb to latest version.
    pub update: bool,
    /// Check status only (default).
    pub check: bool,
    /// Don't prompt, just show what would happen.
    pub dry_run: bool,
    /// Force operation without confirmation.
    pub force: bool,
}

/// Execute the agents command.
///
/// # Errors
///
/// Returns an error if file operations fail.
pub fn execute(args: &AgentsArgs, ctx: &OutputContext) -> Result<()> {
    let work_dir = std::env::current_dir()?;
    let detection = detect_agent_file_in_parents(&work_dir, 3);

    if ctx.is_json() {
        return execute_json(&detection, args, ctx);
    }

    // Default to check mode if no action specified
    let is_check = !args.add && !args.remove && !args.update;

    if is_check || args.check {
        return execute_check(&detection, &work_dir, ctx);
    }

    if args.add {
        return execute_add(&detection, &work_dir, args.dry_run, args.force, ctx);
    }

    if args.remove {
        return execute_remove(&detection, args.dry_run, args.force, ctx);
    }

    if args.update {
        return execute_update(&detection, args.dry_run, args.force, ctx);
    }

    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn execute_json(
    detection: &AgentFileDetection,
    _args: &AgentsArgs,
    ctx: &OutputContext,
) -> Result<()> {
    let output = serde_json::json!({
        "found": detection.found(),
        "file_path": detection.file_path,
        "file_type": detection.file_type,
        "has_blurb": detection.has_blurb,
        "has_legacy_blurb": detection.has_legacy_blurb,
        "blurb_version": detection.blurb_version,
        "current_version": BLURB_VERSION,
        "needs_blurb": detection.needs_blurb(),
        "needs_upgrade": detection.needs_upgrade(),
    });
    ctx.json_pretty(&output);
    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn execute_check(
    detection: &AgentFileDetection,
    work_dir: &Path,
    ctx: &OutputContext,
) -> Result<()> {
    if matches!(ctx.mode(), OutputMode::Rich) {
        render_check_rich(detection, work_dir, ctx);
        return Ok(());
    }

    if !detection.found() {
        println!(
            "No AGENTS.md or CLAUDE.md found in {} or parent directories.",
            work_dir.display()
        );
        println!("\nTo add beads workflow instructions:");
        println!("  br agents --add");
        return Ok(());
    }

    let file_path = detection.file_path.as_ref().unwrap();
    let file_type = detection.file_type.as_ref().unwrap();

    println!("Found: {} at {}", file_type, file_path.display());

    if detection.has_legacy_blurb {
        println!("\nStatus: Contains legacy bv blurb (needs upgrade to br format)");
        println!("\nTo upgrade:");
        println!("  br agents --update");
    } else if detection.has_blurb {
        if detection.blurb_version < BLURB_VERSION {
            println!(
                "\nStatus: Contains br blurb v{} (current: v{})",
                detection.blurb_version, BLURB_VERSION
            );
            println!("\nTo update:");
            println!("  br agents --update");
        } else {
            println!("\nStatus: Contains current br blurb v{BLURB_VERSION}");
        }
    } else {
        println!("\nStatus: No beads workflow instructions found");
        println!("\nTo add:");
        println!("  br agents --add");
    }

    Ok(())
}

fn execute_add(
    detection: &AgentFileDetection,
    work_dir: &Path,
    dry_run: bool,
    force: bool,
    ctx: &OutputContext,
) -> Result<()> {
    // Check if blurb already exists
    if detection.has_blurb
        && !detection.has_legacy_blurb
        && detection.blurb_version >= BLURB_VERSION
    {
        if matches!(ctx.mode(), OutputMode::Rich) {
            render_already_current_rich(ctx);
        } else {
            println!(
                "AGENTS.md already contains current beads workflow instructions (v{BLURB_VERSION})."
            );
        }
        return Ok(());
    }

    let (file_path, content) = if detection.found() {
        let path = detection.file_path.clone().unwrap();
        let content = detection.content.clone().unwrap_or_default();
        (path, content)
    } else {
        // Create new file
        let path = get_preferred_agent_file_path(work_dir);
        let content = String::new();
        (path, content)
    };

    // If has legacy or outdated blurb, do update instead
    if detection.has_legacy_blurb
        || (detection.has_blurb && detection.blurb_version < BLURB_VERSION)
    {
        return execute_update(detection, dry_run, force, ctx);
    }

    let new_content = append_blurb(&content);

    if dry_run {
        if matches!(ctx.mode(), OutputMode::Rich) {
            render_dry_run_add_rich(&file_path, ctx);
        } else {
            println!(
                "Would add beads workflow instructions to: {}",
                file_path.display()
            );
            println!("\n--- Preview ---");
            println!("{AGENT_BLURB}");
        }
        return Ok(());
    }

    // Prompt for confirmation unless forced
    if !force && !detection.found() {
        println!("This will create a new AGENTS.md with beads workflow instructions.");
        println!("File: {}", file_path.display());
        print!("Continue? [y/N] ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Backup existing file
    if detection.found() {
        let backup_path = file_path.with_extension("md.bak");
        if let Err(e) = fs::copy(&file_path, &backup_path) {
            eprintln!(
                "Warning: Could not create backup at {}: {}",
                backup_path.display(),
                e
            );
        } else if !matches!(ctx.mode(), OutputMode::Rich) {
            println!("Backup created: {}", backup_path.display());
        }
    }

    fs::write(&file_path, &new_content)?;
    if matches!(ctx.mode(), OutputMode::Rich) {
        render_add_success_rich(&file_path, new_content.len(), ctx);
    } else {
        println!(
            "Added beads workflow instructions to: {}",
            file_path.display()
        );
    }

    Ok(())
}

fn execute_remove(
    detection: &AgentFileDetection,
    dry_run: bool,
    force: bool,
    ctx: &OutputContext,
) -> Result<()> {
    if !detection.found() {
        return Err(BeadsError::Validation {
            field: "AGENTS.md".to_string(),
            reason: "not found in current directory or parents".to_string(),
        });
    }

    if !detection.has_blurb && !detection.has_legacy_blurb {
        if matches!(ctx.mode(), OutputMode::Rich) {
            render_nothing_to_remove_rich(ctx);
        } else {
            println!("No beads workflow instructions found to remove.");
        }
        return Ok(());
    }

    let file_path = detection.file_path.as_ref().unwrap();
    let content = detection.content.as_ref().unwrap();

    let new_content = if detection.has_legacy_blurb {
        remove_legacy_blurb(content)
    } else {
        remove_blurb(content)
    };

    if dry_run {
        if matches!(ctx.mode(), OutputMode::Rich) {
            render_dry_run_remove_rich(file_path, ctx);
        } else {
            println!(
                "Would remove beads workflow instructions from: {}",
                file_path.display()
            );
        }
        return Ok(());
    }

    // Prompt for confirmation unless forced
    if !force {
        println!(
            "This will remove beads workflow instructions from: {}",
            file_path.display()
        );
        print!("Continue? [y/N] ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Backup
    let backup_path = file_path.with_extension("md.bak");
    if let Err(e) = fs::copy(file_path, &backup_path) {
        eprintln!(
            "Warning: Could not create backup at {}: {}",
            backup_path.display(),
            e
        );
    } else if !matches!(ctx.mode(), OutputMode::Rich) {
        println!("Backup created: {}", backup_path.display());
    }

    fs::write(file_path, new_content)?;
    if matches!(ctx.mode(), OutputMode::Rich) {
        render_remove_success_rich(file_path, ctx);
    } else {
        println!(
            "Removed beads workflow instructions from: {}",
            file_path.display()
        );
    }

    Ok(())
}

fn execute_update(
    detection: &AgentFileDetection,
    dry_run: bool,
    force: bool,
    ctx: &OutputContext,
) -> Result<()> {
    if !detection.found() {
        return Err(BeadsError::Validation {
            field: "AGENTS.md".to_string(),
            reason: "not found in current directory or parents".to_string(),
        });
    }

    if !detection.needs_upgrade() {
        if matches!(ctx.mode(), OutputMode::Rich) {
            render_already_up_to_date_rich(ctx);
        } else {
            println!("Beads workflow instructions are already up to date (v{BLURB_VERSION}).");
        }
        return Ok(());
    }

    let file_path = detection.file_path.as_ref().unwrap();
    let content = detection.content.as_ref().unwrap();
    let new_content = update_blurb(content);

    let from_version = if detection.has_legacy_blurb {
        "bv (legacy)".to_string()
    } else {
        format!("v{}", detection.blurb_version)
    };

    if dry_run {
        if matches!(ctx.mode(), OutputMode::Rich) {
            render_dry_run_update_rich(file_path, &from_version, ctx);
        } else {
            println!(
                "Would update beads workflow instructions from {from_version} to v{BLURB_VERSION}"
            );
            println!("File: {}", file_path.display());
        }
        return Ok(());
    }

    // Prompt for confirmation unless forced
    if !force {
        println!(
            "This will update beads workflow instructions from {from_version} to v{BLURB_VERSION}."
        );
        println!("File: {}", file_path.display());
        print!("Continue? [y/N] ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Backup
    let backup_path = file_path.with_extension("md.bak");
    if let Err(e) = fs::copy(file_path, &backup_path) {
        eprintln!(
            "Warning: Could not create backup at {}: {}",
            backup_path.display(),
            e
        );
    } else if !matches!(ctx.mode(), OutputMode::Rich) {
        println!("Backup created: {}", backup_path.display());
    }

    fs::write(file_path, &new_content)?;
    if matches!(ctx.mode(), OutputMode::Rich) {
        render_update_success_rich(file_path, &from_version, new_content.len(), ctx);
    } else {
        println!(
            "Updated beads workflow instructions to v{} in: {}",
            BLURB_VERSION,
            file_path.display()
        );
    }

    Ok(())
}

// --- Rich output render functions ---

/// Render check result as a rich panel.
fn render_check_rich(detection: &AgentFileDetection, work_dir: &Path, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    if detection.found() {
        let file_path = detection.file_path.as_ref().unwrap();
        let file_type = detection.file_type.as_ref().unwrap();

        content.append_styled("File        ", theme.dimmed.clone());
        content.append_styled(file_type, theme.emphasis.clone());
        content.append("\n");
        content.append_styled("Path        ", theme.dimmed.clone());
        content.append_styled(&file_path.display().to_string(), theme.accent.clone());
        content.append("\n\n");

        if detection.has_legacy_blurb {
            content.append_styled("\u{26A0} ", theme.warning.clone());
            content.append("Contains legacy bv blurb (needs upgrade to br format)\n\n");
            content.append_styled("To upgrade:\n", theme.dimmed.clone());
            content.append_styled("  br agents --update", theme.accent.clone());
        } else if detection.has_blurb {
            if detection.blurb_version < BLURB_VERSION {
                content.append_styled("\u{26A0} ", theme.warning.clone());
                content.append(&format!(
                    "Contains br blurb v{} (current: v{})\n\n",
                    detection.blurb_version, BLURB_VERSION
                ));
                content.append_styled("To update:\n", theme.dimmed.clone());
                content.append_styled("  br agents --update", theme.accent.clone());
            } else {
                content.append_styled("\u{2713} ", theme.success.clone());
                content.append(&format!("Contains current br blurb v{BLURB_VERSION}"));
            }
        } else {
            content.append_styled("\u{2717} ", theme.warning.clone());
            content.append("No beads workflow instructions found\n\n");
            content.append_styled("To add:\n", theme.dimmed.clone());
            content.append_styled("  br agents --add", theme.accent.clone());
        }
    } else {
        content.append_styled("\u{2717} ", theme.warning.clone());
        content.append("No AGENTS.md or CLAUDE.md found in ");
        content.append_styled(&work_dir.display().to_string(), theme.accent.clone());
        content.append("\n\n");
        content.append_styled(
            "To add beads workflow instructions:\n",
            theme.dimmed.clone(),
        );
        content.append_styled("  br agents --add", theme.accent.clone());
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(
            "Agent Instructions",
            theme.panel_title.clone(),
        ))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render "already current" message in rich mode.
fn render_already_current_rich(ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("\u{2713} ", theme.success.clone());
    text.append(&format!(
        "AGENTS.md already contains current beads workflow instructions (v{BLURB_VERSION})."
    ));

    console.print_renderable(&text);
}

/// Render dry-run add preview in rich mode.
fn render_dry_run_add_rich(file_path: &Path, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled(
        "Would add beads workflow instructions to:\n",
        theme.dimmed.clone(),
    );
    content.append_styled(&file_path.display().to_string(), theme.accent.clone());

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Dry Run", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render add success in rich mode.
fn render_add_success_rich(file_path: &Path, _bytes: usize, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("\u{2713} ", theme.success.clone());
    text.append("Added beads workflow instructions to: ");
    text.append_styled(&file_path.display().to_string(), theme.accent.clone());

    console.print_renderable(&text);
}

/// Render "nothing to remove" message in rich mode.
fn render_nothing_to_remove_rich(ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("\u{2713} ", theme.success.clone());
    text.append("No beads workflow instructions found to remove.");

    console.print_renderable(&text);
}

/// Render dry-run remove preview in rich mode.
fn render_dry_run_remove_rich(file_path: &Path, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled(
        "Would remove beads workflow instructions from:\n",
        theme.dimmed.clone(),
    );
    content.append_styled(&file_path.display().to_string(), theme.accent.clone());

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Dry Run", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render remove success in rich mode.
fn render_remove_success_rich(file_path: &Path, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("\u{2713} ", theme.success.clone());
    text.append("Removed beads workflow instructions from: ");
    text.append_styled(&file_path.display().to_string(), theme.accent.clone());

    console.print_renderable(&text);
}

/// Render "already up to date" message in rich mode.
fn render_already_up_to_date_rich(ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("\u{2713} ", theme.success.clone());
    text.append(&format!(
        "Beads workflow instructions are already up to date (v{BLURB_VERSION})."
    ));

    console.print_renderable(&text);
}

/// Render dry-run update preview in rich mode.
fn render_dry_run_update_rich(file_path: &Path, from_version: &str, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled(
        "Would update beads workflow instructions from ",
        theme.dimmed.clone(),
    );
    content.append_styled(from_version, theme.warning.clone());
    content.append_styled(" to ", theme.dimmed.clone());
    content.append_styled(&format!("v{BLURB_VERSION}"), theme.success.clone());
    content.append("\n");
    content.append_styled("File: ", theme.dimmed.clone());
    content.append_styled(&file_path.display().to_string(), theme.accent.clone());

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Dry Run", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render update success in rich mode.
fn render_update_success_rich(
    file_path: &Path,
    from_version: &str,
    _bytes: usize,
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("\u{2713} ", theme.success.clone());
    text.append("Updated beads workflow instructions from ");
    text.append_styled(from_version, theme.warning.clone());
    text.append(" to ");
    text.append_styled(&format!("v{BLURB_VERSION}"), theme.success.clone());
    text.append(" in: ");
    text.append_styled(&file_path.display().to_string(), theme.accent.clone());

    console.print_renderable(&text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_contains_blurb() {
        let content = "Some text\n<!-- br-agent-instructions-v1 -->\nblurb\n<!-- end-br-agent-instructions -->";
        assert!(contains_blurb(content));
        assert!(!contains_legacy_blurb(content));
    }

    #[test]
    fn test_contains_legacy_blurb() {
        let content = "Some text\n<!-- bv-agent-instructions-v1 -->\nblurb\n<!-- end-bv-agent-instructions -->";
        assert!(!contains_blurb(content));
        assert!(contains_legacy_blurb(content));
        assert!(contains_any_blurb(content));
    }

    #[test]
    fn test_get_blurb_version() {
        assert_eq!(get_blurb_version("<!-- br-agent-instructions-v1 -->"), 1);
        assert_eq!(get_blurb_version("<!-- br-agent-instructions-v2 -->"), 2);
        assert_eq!(get_blurb_version("no marker"), 0);
    }

    #[test]
    fn test_detect_agent_file() {
        let temp_dir = TempDir::new().unwrap();

        // No file exists
        let detection = detect_agent_file(temp_dir.path());
        assert!(!detection.found());

        // Create AGENTS.md
        let agents_path = temp_dir.path().join("AGENTS.md");
        fs::write(&agents_path, "# Agents\n").unwrap();

        let detection = detect_agent_file(temp_dir.path());
        assert!(detection.found());
        assert_eq!(detection.file_type.as_deref(), Some("AGENTS.md"));
        assert!(!detection.has_blurb);
    }

    #[test]
    fn test_detect_agent_file_with_blurb() {
        let temp_dir = TempDir::new().unwrap();
        let content = format!("# Agents\n\n{AGENT_BLURB}\n");
        let agents_path = temp_dir.path().join("AGENTS.md");
        fs::write(&agents_path, content).unwrap();

        let detection = detect_agent_file(temp_dir.path());
        assert!(detection.found());
        assert!(detection.has_blurb);
        assert_eq!(detection.blurb_version, 1);
        assert!(!detection.needs_blurb());
        assert!(!detection.needs_upgrade());
    }

    #[test]
    fn test_append_blurb() {
        let content = "# Agents\n\nSome content.";
        let result = append_blurb(content);
        assert!(result.contains(BLURB_START_MARKER));
        assert!(result.contains(BLURB_END_MARKER));
        assert!(result.starts_with("# Agents"));
    }

    #[test]
    fn test_remove_blurb() {
        let content = format!("# Agents\n\n{AGENT_BLURB}\n\nMore content.");
        let result = remove_blurb(&content);
        assert!(!result.contains(BLURB_START_MARKER));
        assert!(result.contains("# Agents"));
        assert!(result.contains("More content."));
    }

    #[test]
    fn test_update_blurb() {
        // Test updating legacy bv blurb
        let legacy_content = "# Agents\n\n<!-- bv-agent-instructions-v1 -->\nold\n<!-- end-bv-agent-instructions -->\n";
        let result = update_blurb(legacy_content);
        assert!(!result.contains("bv-agent-instructions"));
        assert!(result.contains("br-agent-instructions-v1"));
    }

    #[test]
    fn test_detect_in_parents() {
        let temp_dir = TempDir::new().unwrap();
        let sub_dir = temp_dir.path().join("subdir");
        fs::create_dir(&sub_dir).unwrap();

        // Create AGENTS.md in parent
        let agents_path = temp_dir.path().join("AGENTS.md");
        fs::write(&agents_path, "# Agents\n").unwrap();

        // Should find it from subdir
        let detection = detect_agent_file_in_parents(&sub_dir, 3);
        assert!(detection.found());
        assert_eq!(detection.file_path.unwrap(), agents_path);
    }
}
