//! AGENTS.md blurb detection and management.
//!
//! This module provides functionality to detect, add, update, and remove
//! beads workflow instructions in AGENTS.md or CLAUDE.md files.

use crate::error::{BeadsError, Result};
use crate::output::{OutputContext, OutputMode};
use regex::Regex;
use rich_rust::prelude::*;
use std::fs::{self, File, OpenOptions};
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
# View ready issues (open, unblocked, not deferred)
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

- **Dependencies**: Issues can block other issues. `br ready` shows only open, unblocked work.
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
    /// Whether the file exists but was unreadable.
    pub read_error: Option<String>,
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

    /// Returns true if the file exists but could not be read.
    #[must_use]
    pub const fn unreadable(&self) -> bool {
        self.read_error.is_some()
    }

    /// Returns true if the file exists but doesn't have our blurb.
    #[must_use]
    pub const fn needs_blurb(&self) -> bool {
        self.found() && !self.unreadable() && !self.has_blurb
    }

    /// Returns true if the file has an older version that needs upgrade.
    #[must_use]
    pub const fn needs_upgrade(&self) -> bool {
        if self.has_legacy_blurb {
            return true;
        }
        self.has_blurb && self.blurb_version < BLURB_VERSION
    }

    fn file_path_ref(&self, context: &str) -> Result<&Path> {
        self.file_path.as_deref().ok_or_else(|| {
            BeadsError::internal(format!(
                "agent file detection is missing file_path while handling {context}"
            ))
        })
    }

    fn file_type_ref(&self, context: &str) -> Result<&str> {
        self.file_type.as_deref().ok_or_else(|| {
            BeadsError::internal(format!(
                "agent file detection is missing file_type while handling {context}"
            ))
        })
    }

    fn content_ref(&self, context: &str) -> Result<&str> {
        self.content.as_deref().ok_or_else(|| {
            BeadsError::internal(format!(
                "agent file detection is missing content while handling {context}"
            ))
        })
    }
}

#[must_use]
const fn inferred_dry_run_action(detection: &AgentFileDetection) -> &'static str {
    if !detection.found() {
        "create"
    } else if detection.needs_upgrade() {
        "update"
    } else if detection.needs_blurb() {
        "add"
    } else {
        "none"
    }
}

/// Check if content contains the br agent blurb.
#[must_use]
pub fn contains_blurb(content: &str) -> bool {
    contains_marker_block(content, "<!-- br-agent-instructions-v", BLURB_END_MARKER)
}

/// Check if content contains the legacy bv blurb.
#[must_use]
pub fn contains_legacy_blurb(content: &str) -> bool {
    contains_marker_block(
        content,
        "<!-- bv-agent-instructions-v",
        "<!-- end-bv-agent-instructions -->",
    )
}

/// Check if content contains any blurb (br or bv).
#[must_use]
pub fn contains_any_blurb(content: &str) -> bool {
    contains_blurb(content) || contains_legacy_blurb(content)
}

static BLURB_VERSION_REGEX: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"<!-- br-agent-instructions-v(\d+) -->")
        .expect("static regex compilation must not fail")
});

/// Extract the version number from an existing blurb.
#[must_use]
pub fn get_blurb_version(content: &str) -> u8 {
    let Some((start_idx, end_idx)) =
        find_marker_block_range(content, "<!-- br-agent-instructions-v", BLURB_END_MARKER)
    else {
        return 0;
    };

    if let Some(caps) = BLURB_VERSION_REGEX.captures(&content[start_idx..end_idx])
        && let Some(m) = caps.get(1)
    {
        return m.as_str().parse().unwrap_or(0);
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
    check_agent_file_with_reader(file_path, file_type, |path| fs::read_to_string(path))
}

fn check_agent_file_with_reader<R>(
    file_path: &Path,
    file_type: &str,
    read_to_string: R,
) -> Option<AgentFileDetection>
where
    R: for<'a> Fn(&'a Path) -> io::Result<String>,
{
    let metadata = match fs::symlink_metadata(file_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            return Some(AgentFileDetection {
                file_path: Some(file_path.to_path_buf()),
                file_type: Some(file_type.to_string()),
                read_error: Some(err.to_string()),
                ..Default::default()
            });
        }
    };

    if metadata.file_type().is_symlink() {
        return Some(AgentFileDetection {
            file_path: Some(file_path.to_path_buf()),
            file_type: Some(file_type.to_string()),
            read_error: Some(
                "symbolic links are not supported for agent instruction files".to_string(),
            ),
            ..Default::default()
        });
    }

    if metadata.is_dir() {
        return None;
    }

    let content = match read_to_string(file_path) {
        Ok(content) => content,
        Err(err) => {
            return Some(AgentFileDetection {
                file_path: Some(file_path.to_path_buf()),
                file_type: Some(file_type.to_string()),
                read_error: Some(err.to_string()),
                ..Default::default()
            });
        }
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
        read_error: None,
    })
}

/// Detect an agent file, searching parent directories.
#[must_use]
pub fn detect_agent_file_in_parents(work_dir: &Path, max_levels: usize) -> AgentFileDetection {
    let mut current_dir = work_dir.to_path_buf();
    let mut levels = 0;

    loop {
        let detection = detect_agent_file(&current_dir);
        if detection.found() {
            return detection;
        }

        if levels == max_levels {
            break;
        }

        // Move to parent
        match current_dir.parent() {
            Some(parent) if parent != current_dir => {
                current_dir = parent.to_path_buf();
                levels += 1;
            }
            _ => break, // Reached root
        }
    }

    AgentFileDetection::default()
}

fn find_agent_search_root(work_dir: &Path) -> PathBuf {
    let mut current_dir = work_dir.to_path_buf();

    loop {
        let is_project_root = current_dir.join(".git").exists()
            || current_dir.join(".beads").is_dir()
            || current_dir.join("_beads").is_dir();
        if is_project_root {
            return current_dir;
        }

        match current_dir.parent() {
            Some(parent) if parent != current_dir => {
                current_dir = parent.to_path_buf();
            }
            _ => break,
        }
    }

    work_dir.to_path_buf()
}

/// Detect an agent file within the current project boundary.
///
/// Searches upward from the working directory to the nearest project root
/// marker (`.git`, `.beads`, or `_beads`). If no project marker exists, only
/// the current directory is searched to avoid capturing unrelated files above
/// the active workspace.
#[must_use]
pub fn detect_agent_file_in_project(work_dir: &Path) -> AgentFileDetection {
    let search_root = find_agent_search_root(work_dir);
    let mut current_dir = work_dir.to_path_buf();

    loop {
        let detection = detect_agent_file(&current_dir);
        if detection.found() {
            return detection;
        }

        if current_dir == search_root {
            break;
        }

        match current_dir.parent() {
            Some(parent) if parent != current_dir => {
                current_dir = parent.to_path_buf();
            }
            _ => break,
        }
    }

    AgentFileDetection::default()
}

/// Append the blurb to content.
#[must_use]
pub fn append_blurb(content: &str) -> String {
    if content.is_empty() {
        return format!("{AGENT_BLURB}\n");
    }

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
    let Some((start_idx, end_idx)) =
        find_marker_block_range(content, start_marker, BLURB_END_MARKER)
    else {
        return content.to_string();
    };
    remove_marker_block(content, start_idx, end_idx)
}

/// Remove legacy bv blurb from content.
#[must_use]
pub fn remove_legacy_blurb(content: &str) -> String {
    if !contains_legacy_blurb(content) {
        return content.to_string();
    }

    let start_marker = "<!-- bv-agent-instructions-v";
    let end_marker = "<!-- end-bv-agent-instructions -->";

    let Some((start_idx, end_idx)) = find_marker_block_range(content, start_marker, end_marker)
    else {
        return content.to_string();
    };
    remove_marker_block(content, start_idx, end_idx)
}

/// Update an existing blurb to the current version.
#[must_use]
pub fn update_blurb(content: &str) -> String {
    let content = remove_legacy_blurb(content);
    let content = remove_blurb(&content);
    append_blurb(&content)
}

fn remove_all_agent_blurbs(content: &str) -> String {
    remove_blurb(&remove_legacy_blurb(content))
}

fn find_marker_end_after(content: &str, start_idx: usize, end_marker: &str) -> Option<usize> {
    content[start_idx..]
        .find(end_marker)
        .map(|relative_end| start_idx + relative_end + end_marker.len())
}

fn find_marker_block_range(
    content: &str,
    start_marker: &str,
    end_marker: &str,
) -> Option<(usize, usize)> {
    let mut search_from = 0;

    while search_from < content.len() {
        let relative_start = content[search_from..].find(start_marker)?;
        let start_idx = search_from + relative_start;
        let next_search_from = start_idx + start_marker.len();
        let next_start_idx = content[next_search_from..]
            .find(start_marker)
            .map(|relative_next| next_search_from + relative_next);

        if let Some(end_idx) = find_marker_end_after(content, start_idx, end_marker)
            && next_start_idx.is_none_or(|next_start| end_idx <= next_start)
        {
            return Some((start_idx, end_idx));
        }

        search_from = next_search_from;
    }

    None
}

fn contains_marker_block(content: &str, start_marker: &str, end_marker: &str) -> bool {
    find_marker_block_range(content, start_marker, end_marker).is_some()
}

fn remove_marker_block(content: &str, start_idx: usize, end_idx: usize) -> String {
    let before = content[..start_idx].trim_end_matches(['\r', '\n']);
    let after = content[end_idx..].trim_start_matches(['\r', '\n']);

    if before.is_empty() {
        return after.to_string();
    }

    if after.is_empty() {
        return before.to_string();
    }

    let newline = if content[..start_idx].contains("\r\n") || content[end_idx..].contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };

    format!("{before}{newline}{newline}{after}")
}

/// Get the preferred path for a new agent file.
#[must_use]
pub fn get_preferred_agent_file_path(work_dir: &Path) -> PathBuf {
    work_dir.join("AGENTS.md")
}

fn require_force_for_json_action(force: bool, ctx: &OutputContext) -> Result<()> {
    if ctx.is_json() && !force {
        return Err(BeadsError::Validation {
            field: "force".to_string(),
            reason: "--force is required for mutating `br agents` actions in JSON mode".to_string(),
        });
    }

    Ok(())
}

fn create_temp_agent_file(file_path: &Path) -> Result<(PathBuf, File)> {
    let pid = std::process::id();
    let base_extension = file_path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("tmp");

    for attempt in 0..64_u32 {
        let extension = if attempt == 0 {
            format!("{base_extension}.{pid}.tmp")
        } else {
            format!("{base_extension}.{pid}.{attempt}.tmp")
        };
        let temp_path = file_path.with_extension(extension);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }

    Err(BeadsError::Config(format!(
        "Failed to allocate temp agent file for {}",
        file_path.display()
    )))
}

fn write_new_agent_file(file_path: &Path, contents: &[u8]) -> Result<()> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file_path)
    {
        Ok(mut file) => {
            if let Err(error) = file.write_all(contents).and_then(|()| file.sync_all()) {
                drop(file);
                let _ = fs::remove_file(file_path);
                return Err(error.into());
            }
            drop(file);
            crate::util::sync_parent_directory(file_path)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Err(BeadsError::Config(format!(
                "Agent instruction file already exists: {}",
                file_path.display()
            )))
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_agent_rewrite_target(file_path: &Path) -> Result<Option<fs::Permissions>> {
    match fs::symlink_metadata(file_path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(BeadsError::Config(format!(
                    "Agent instruction file '{}' must not be a symlink",
                    file_path.display()
                )));
            }
            if !file_type.is_file() {
                return Err(BeadsError::Config(format!(
                    "Agent instruction file '{}' must be a regular file",
                    file_path.display()
                )));
            }
            Ok(Some(metadata.permissions()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_agent_file_atomically(file_path: &Path, contents: &[u8]) -> Result<()> {
    let existing_permissions = validate_agent_rewrite_target(file_path)?;
    let (temp_path, mut temp_file) = create_temp_agent_file(file_path)?;
    if let Some(permissions) = existing_permissions
        && let Err(error) = fs::set_permissions(&temp_path, permissions)
    {
        tracing::warn!(
            path = %file_path.display(),
            error = %error,
            "Failed to apply original agent file permissions before atomic rewrite"
        );
    }
    if let Err(error) = temp_file
        .write_all(contents)
        .and_then(|()| temp_file.sync_all())
    {
        drop(temp_file);
        let _ = fs::remove_file(&temp_path);
        return Err(error.into());
    }
    drop(temp_file);
    crate::util::durable_rename(&temp_path, file_path).inspect_err(|_| {
        let _ = fs::remove_file(&temp_path);
    })?;
    Ok(())
}

fn write_added_agent_file(
    file_path: &Path,
    new_content: &str,
    replacing_existing_file: bool,
) -> Result<()> {
    if replacing_existing_file {
        write_agent_file_atomically(file_path, new_content.as_bytes())
    } else {
        write_new_agent_file(file_path, new_content.as_bytes())
    }
}

fn read_agent_file_for_backup(file_path: &Path) -> Result<Vec<u8>> {
    validate_agent_rewrite_target(file_path)?;
    fs::read(file_path).map_err(Into::into)
}

fn backup_agent_file(file_path: &Path, ctx: &OutputContext) -> Option<PathBuf> {
    let backup_path = file_path.with_extension("md.bak");
    let backup_bytes = match read_agent_file_for_backup(file_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!(
                "Warning: Could not read {} for backup: {}",
                file_path.display(),
                e
            );
            return None;
        }
    };
    match write_agent_file_atomically(&backup_path, &backup_bytes) {
        Ok(()) => {
            if !ctx.is_json() && !matches!(ctx.mode(), OutputMode::Rich) {
                println!("Backup created: {}", backup_path.display());
            }
            Some(backup_path)
        }
        Err(e) => {
            eprintln!(
                "Warning: Could not create backup at {}: {}",
                backup_path.display(),
                e
            );
            None
        }
    }
}

fn add_confirmation_message(detection: &AgentFileDetection, file_path: &Path) -> String {
    if detection.found() {
        format!(
            "This will add beads workflow instructions to: {}",
            file_path.display()
        )
    } else {
        format!(
            "This will create a new AGENTS.md with beads workflow instructions.\nFile: {}",
            file_path.display()
        )
    }
}

fn confirm_add_operation(
    detection: &AgentFileDetection,
    file_path: &Path,
    force: bool,
) -> Result<bool> {
    if force {
        return Ok(true);
    }

    println!("{}", add_confirmation_message(detection, file_path));
    print!("Continue? [y/N] ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

fn validate_action_selection(args: &AgentsArgs) -> Result<()> {
    let selected_actions = [args.add, args.remove, args.update, args.check]
        .into_iter()
        .filter(|selected| *selected)
        .count();

    if selected_actions > 1 {
        return Err(BeadsError::Validation {
            field: "action".to_string(),
            reason: "choose only one of --add, --remove, --update, or --check".to_string(),
        });
    }

    Ok(())
}

fn search_scope_description(work_dir: &Path) -> String {
    let search_root = find_agent_search_root(work_dir);
    if search_root == work_dir {
        format!("in {}", work_dir.display())
    } else {
        format!(
            "between {} and project root {}",
            work_dir.display(),
            search_root.display()
        )
    }
}

fn agent_file_not_found_reason(work_dir: &Path) -> String {
    let search_root = find_agent_search_root(work_dir);
    if search_root == work_dir {
        format!("not found in current directory ({})", work_dir.display())
    } else {
        format!(
            "not found between current directory ({}) and project root ({})",
            work_dir.display(),
            search_root.display()
        )
    }
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
    validate_action_selection(args)?;
    let work_dir = std::env::current_dir()?;
    execute_validated_from(args, ctx, &work_dir)
}

#[cfg(test)]
fn execute_from(args: &AgentsArgs, ctx: &OutputContext, work_dir: &Path) -> Result<()> {
    validate_action_selection(args)?;
    execute_validated_from(args, ctx, work_dir)
}

fn execute_validated_from(args: &AgentsArgs, ctx: &OutputContext, work_dir: &Path) -> Result<()> {
    let detection = detect_agent_file_in_project(work_dir);

    // Default to check mode if no action specified
    let is_check = !args.add && !args.remove && !args.update;

    // When --dry-run is passed without an explicit action, infer the action
    // from the current state so the user sees what *would* happen.
    if args.dry_run && is_check {
        if ctx.is_json() {
            return execute_json(&detection, args, work_dir, ctx);
        }
        return execute_dry_run_inferred(&detection, work_dir, ctx);
    }

    if is_check || args.check {
        if ctx.is_json() {
            return execute_json(&detection, args, work_dir, ctx);
        }
        return execute_check(&detection, work_dir, ctx);
    }

    if args.add {
        return execute_add(&detection, work_dir, args.dry_run, args.force, ctx);
    }

    if args.remove {
        return execute_remove(&detection, work_dir, args.dry_run, args.force, ctx);
    }

    if args.update {
        return execute_update(&detection, work_dir, args.dry_run, args.force, ctx);
    }

    Ok(())
}

/// Dry-run without an explicit action: infer what would happen and display it.
#[allow(clippy::unnecessary_wraps)]
fn execute_dry_run_inferred(
    detection: &AgentFileDetection,
    work_dir: &Path,
    ctx: &OutputContext,
) -> Result<()> {
    let is_rich = matches!(ctx.mode(), OutputMode::Rich);

    if !detection.found() {
        // No agent file exists -- would create AGENTS.md with blurb
        let target_path = get_preferred_agent_file_path(work_dir);
        if is_rich {
            render_dry_run_add_rich(&target_path, ctx);
            // Also show the blurb preview in rich mode
            let console = Console::default();
            let theme = ctx.theme();
            let width = ctx.width();

            let mut content = Text::new("");
            content.append_styled(
                "Preview of content that would be added:\n\n",
                theme.dimmed.clone(),
            );
            // Show a truncated preview (first few lines)
            for line in AGENT_BLURB.lines().take(12) {
                content.append_styled(line, theme.dimmed.clone());
                content.append("\n");
            }
            content.append_styled("  ... (", theme.dimmed.clone());
            content.append_styled(
                &format!("{} lines total", AGENT_BLURB.lines().count()),
                theme.emphasis.clone(),
            );
            content.append_styled(")\n", theme.dimmed.clone());

            let panel = Panel::from_rich_text(&content, width)
                .title(Text::styled("Blurb Preview", theme.panel_title.clone()))
                .box_style(theme.box_style);
            console.print_renderable(&panel);
        } else {
            println!(
                "Dry-run: would create {} with beads workflow instructions",
                target_path.display()
            );
            println!("\n--- Preview ---");
            println!("{AGENT_BLURB}");
        }
        return Ok(());
    }

    if let Some(err) = detection.read_error.as_deref() {
        let file_path = detection.file_path_ref("agents dry-run unreadable file")?;
        let file_type = detection.file_type.as_deref().unwrap_or("agent file");
        if is_rich {
            render_unreadable_rich(file_path, file_type, err, "Dry Run", ctx);
        } else {
            println!(
                "Dry-run: cannot determine action because {} at {} is unreadable: {}",
                file_type,
                file_path.display(),
                err
            );
        }
        return Ok(());
    }

    if detection.needs_upgrade() {
        // Would update existing blurb
        let file_path = detection.file_path_ref("agents dry-run update")?;
        let from_version = if detection.has_legacy_blurb {
            "bv (legacy)".to_string()
        } else {
            format!("v{}", detection.blurb_version)
        };
        if is_rich {
            render_dry_run_update_rich(file_path, &from_version, ctx);
        } else {
            println!(
                "Dry-run: would update beads workflow instructions from {from_version} to v{BLURB_VERSION}"
            );
            println!("File: {}", file_path.display());
        }
        return Ok(());
    }

    if detection.needs_blurb() {
        // File exists but has no blurb -- would add
        let file_path = detection.file_path_ref("agents dry-run add")?;
        if is_rich {
            render_dry_run_add_rich(file_path, ctx);
        } else {
            println!(
                "Dry-run: would add beads workflow instructions to {}",
                file_path.display()
            );
            println!("\n--- Preview ---");
            println!("{AGENT_BLURB}");
        }
        return Ok(());
    }

    // Already up to date -- nothing to do
    if is_rich {
        render_already_up_to_date_rich(ctx);
    } else {
        println!(
            "Dry-run: no changes needed. Beads workflow instructions are up to date (v{BLURB_VERSION})."
        );
    }

    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn execute_json(
    detection: &AgentFileDetection,
    args: &AgentsArgs,
    work_dir: &Path,
    ctx: &OutputContext,
) -> Result<()> {
    if args.dry_run {
        let would_action = inferred_dry_run_action(detection);

        let target_path = if detection.found() {
            detection.file_path.clone()
        } else {
            Some(get_preferred_agent_file_path(work_dir))
        };

        let output = serde_json::json!({
            "dry_run": true,
            "found": detection.found(),
            "file_path": target_path,
            "file_type": detection.file_type,
            "has_blurb": detection.has_blurb,
            "has_legacy_blurb": detection.has_legacy_blurb,
            "blurb_version": detection.blurb_version,
            "current_version": BLURB_VERSION,
            "read_error": detection.read_error,
            "needs_blurb": detection.needs_blurb(),
            "needs_upgrade": detection.needs_upgrade(),
            "would_action": would_action,
        });
        ctx.json_pretty(&output);
        return Ok(());
    }

    let output = serde_json::json!({
        "found": detection.found(),
        "file_path": detection.file_path,
        "file_type": detection.file_type,
        "has_blurb": detection.has_blurb,
        "has_legacy_blurb": detection.has_legacy_blurb,
        "blurb_version": detection.blurb_version,
        "current_version": BLURB_VERSION,
        "read_error": detection.read_error,
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
        return render_check_rich(detection, work_dir, ctx);
    }

    if !detection.found() {
        println!(
            "No AGENTS.md or CLAUDE.md found {}.",
            search_scope_description(work_dir)
        );
        println!("\nTo add beads workflow instructions:");
        println!("  br agents --add");
        return Ok(());
    }

    let file_path = detection.file_path_ref("agents check")?;
    let file_type = detection.file_type_ref("agents check")?;

    println!("Found: {} at {}", file_type, file_path.display());

    if let Some(err) = detection.read_error.as_deref() {
        println!("\nStatus: File is unreadable");
        println!("Error: {err}");
    } else if detection.has_legacy_blurb {
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
        if ctx.is_json() {
            let output = serde_json::json!({
                "action": "add",
                "performed": false,
                "reason": "already_current",
                "file_path": detection.file_path.as_ref().map(|path| path.display().to_string()),
                "current_version": BLURB_VERSION,
            });
            ctx.json_pretty(&output);
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_already_current_rich(ctx);
        } else {
            println!(
                "AGENTS.md already contains current beads workflow instructions (v{BLURB_VERSION})."
            );
        }
        return Ok(());
    }

    let (file_path, content) = if detection.found() {
        if let Some(ref err) = detection.read_error {
            return Err(BeadsError::Config(format!(
                "Cannot add instructions: {} exists but is unreadable: {}",
                detection.file_type_ref("agents add unreadable file")?,
                err
            )));
        }
        let path = detection
            .file_path_ref("agents add existing file")?
            .to_path_buf();
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
        return execute_update(detection, work_dir, dry_run, force, ctx);
    }

    let new_content = append_blurb(&content);

    if dry_run {
        if ctx.is_json() {
            let output = serde_json::json!({
                "dry_run": true,
                "action": "add",
                "file_path": file_path.display().to_string(),
                "created_file": !detection.found(),
                "current_version": BLURB_VERSION,
                "would_action": "add",
            });
            ctx.json_pretty(&output);
        } else if matches!(ctx.mode(), OutputMode::Rich) {
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

    require_force_for_json_action(force, ctx)?;

    if !confirm_add_operation(detection, &file_path, force)? {
        println!("Aborted.");
        return Ok(());
    }

    // Backup existing file
    let backup_path = detection
        .found()
        .then(|| backup_agent_file(&file_path, ctx))
        .flatten();

    write_added_agent_file(&file_path, &new_content, detection.found())?;
    if ctx.is_json() {
        let output = serde_json::json!({
            "action": "add",
            "performed": true,
            "file_path": file_path.display().to_string(),
            "backup_path": backup_path.as_ref().map(|path| path.display().to_string()),
            "created_file": !detection.found(),
            "bytes_written": new_content.len(),
            "current_version": BLURB_VERSION,
        });
        ctx.json_pretty(&output);
    } else if matches!(ctx.mode(), OutputMode::Rich) {
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
    work_dir: &Path,
    dry_run: bool,
    force: bool,
    ctx: &OutputContext,
) -> Result<()> {
    if !detection.found() {
        return Err(BeadsError::Validation {
            field: "AGENTS.md".to_string(),
            reason: agent_file_not_found_reason(work_dir),
        });
    }

    if let Some(ref err) = detection.read_error {
        return Err(BeadsError::Config(format!(
            "Cannot remove instructions: {} exists but is unreadable: {}",
            detection.file_type_ref("agents remove unreadable file")?,
            err
        )));
    }

    if !detection.has_blurb && !detection.has_legacy_blurb {
        if ctx.is_json() {
            let output = serde_json::json!({
                "action": "remove",
                "performed": false,
                "reason": "nothing_to_remove",
                "file_path": detection.file_path.as_ref().map(|path| path.display().to_string()),
                "current_version": BLURB_VERSION,
            });
            ctx.json_pretty(&output);
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_nothing_to_remove_rich(ctx);
        } else {
            println!("No beads workflow instructions found to remove.");
        }
        return Ok(());
    }

    let file_path = detection.file_path_ref("agents remove")?;
    let content = detection.content_ref("agents remove")?;

    let new_content = remove_all_agent_blurbs(content);

    if dry_run {
        if ctx.is_json() {
            let output = serde_json::json!({
                "dry_run": true,
                "action": "remove",
                "file_path": file_path.display().to_string(),
                "current_version": BLURB_VERSION,
                "would_action": "remove",
            });
            ctx.json_pretty(&output);
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_dry_run_remove_rich(file_path, ctx);
        } else {
            println!(
                "Would remove beads workflow instructions from: {}",
                file_path.display()
            );
        }
        return Ok(());
    }

    require_force_for_json_action(force, ctx)?;

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
    let backup_path = backup_agent_file(file_path, ctx);

    write_agent_file_atomically(file_path, new_content.as_bytes())?;
    if ctx.is_json() {
        let output = serde_json::json!({
            "action": "remove",
            "performed": true,
            "file_path": file_path.display().to_string(),
            "backup_path": backup_path.as_ref().map(|path| path.display().to_string()),
            "bytes_written": new_content.len(),
            "current_version": BLURB_VERSION,
        });
        ctx.json_pretty(&output);
    } else if matches!(ctx.mode(), OutputMode::Rich) {
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
    work_dir: &Path,
    dry_run: bool,
    force: bool,
    ctx: &OutputContext,
) -> Result<()> {
    if !detection.found() {
        return Err(BeadsError::Validation {
            field: "AGENTS.md".to_string(),
            reason: agent_file_not_found_reason(work_dir),
        });
    }

    if let Some(ref err) = detection.read_error {
        return Err(BeadsError::Config(format!(
            "Cannot update instructions: {} exists but is unreadable: {}",
            detection.file_type_ref("agents update unreadable file")?,
            err
        )));
    }

    if !detection.needs_upgrade() {
        if ctx.is_json() {
            let output = serde_json::json!({
                "action": "update",
                "performed": false,
                "reason": "already_up_to_date",
                "file_path": detection.file_path.as_ref().map(|path| path.display().to_string()),
                "current_version": BLURB_VERSION,
            });
            ctx.json_pretty(&output);
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_already_up_to_date_rich(ctx);
        } else {
            println!("Beads workflow instructions are already up to date (v{BLURB_VERSION}).");
        }
        return Ok(());
    }

    let file_path = detection.file_path_ref("agents update")?;
    let content = detection.content_ref("agents update")?;
    let new_content = update_blurb(content);

    let from_version = if detection.has_legacy_blurb {
        "bv (legacy)".to_string()
    } else {
        format!("v{}", detection.blurb_version)
    };

    if dry_run {
        if ctx.is_json() {
            let output = serde_json::json!({
                "dry_run": true,
                "action": "update",
                "file_path": file_path.display().to_string(),
                "from_version": from_version,
                "to_version": BLURB_VERSION,
                "would_action": "update",
            });
            ctx.json_pretty(&output);
        } else if matches!(ctx.mode(), OutputMode::Rich) {
            render_dry_run_update_rich(file_path, &from_version, ctx);
        } else {
            println!(
                "Would update beads workflow instructions from {from_version} to v{BLURB_VERSION}"
            );
            println!("File: {}", file_path.display());
        }
        return Ok(());
    }

    require_force_for_json_action(force, ctx)?;

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
    let backup_path = backup_agent_file(file_path, ctx);

    write_agent_file_atomically(file_path, new_content.as_bytes())?;
    if ctx.is_json() {
        let output = serde_json::json!({
            "action": "update",
            "performed": true,
            "file_path": file_path.display().to_string(),
            "backup_path": backup_path.as_ref().map(|path| path.display().to_string()),
            "from_version": from_version,
            "to_version": BLURB_VERSION,
            "bytes_written": new_content.len(),
        });
        ctx.json_pretty(&output);
    } else if matches!(ctx.mode(), OutputMode::Rich) {
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
fn render_check_rich(
    detection: &AgentFileDetection,
    work_dir: &Path,
    ctx: &OutputContext,
) -> Result<()> {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    if detection.found() {
        let file_path = detection.file_path_ref("agents check rich")?;
        let file_type = detection.file_type_ref("agents check rich")?;

        content.append_styled("File        ", theme.dimmed.clone());
        content.append_styled(file_type, theme.emphasis.clone());
        content.append("\n");
        content.append_styled("Path        ", theme.dimmed.clone());
        content.append_styled(&file_path.display().to_string(), theme.accent.clone());
        content.append("\n\n");

        if let Some(err) = detection.read_error.as_deref() {
            content.append_styled("\u{26A0} ", theme.warning.clone());
            content.append("Agent instructions file is unreadable\n\n");
            content.append_styled("Error:\n", theme.dimmed.clone());
            content.append_styled(err, theme.warning.clone());
        } else if detection.has_legacy_blurb {
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
        content.append("No AGENTS.md or CLAUDE.md found ");
        content.append_styled(&search_scope_description(work_dir), theme.accent.clone());
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
    Ok(())
}

fn render_unreadable_rich(
    file_path: &Path,
    file_type: &str,
    err: &str,
    title: &str,
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled(
        "Cannot determine what would happen because the agent file is unreadable.\n\n",
        theme.warning.clone(),
    );
    content.append_styled("File        ", theme.dimmed.clone());
    content.append_styled(file_type, theme.emphasis.clone());
    content.append("\n");
    content.append_styled("Path        ", theme.dimmed.clone());
    content.append_styled(&file_path.display().to_string(), theme.accent.clone());
    content.append("\n");
    content.append_styled("Error       ", theme.dimmed.clone());
    content.append_styled(err, theme.warning.clone());

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(title, theme.panel_title.clone()))
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
    use crate::output::OutputContext;

    use tempfile::TempDir;

    fn assert_unexpected_error(other: &BeadsError) {
        let message = format!("{other:?}");
        assert!(message.is_empty(), "unexpected error: {message}");
    }

    fn isolated_agent_project() -> TempDir {
        let temp_dir = TempDir::new().expect("create isolated agent project");
        fs::create_dir(temp_dir.path().join(".git")).expect("mark isolated project root");
        temp_dir
    }

    #[test]
    fn test_contains_blurb() {
        let content = "Some text\n<!-- br-agent-instructions-v1 -->\nblurb\n<!-- end-br-agent-instructions -->";
        assert!(contains_blurb(content));
        assert!(!contains_legacy_blurb(content));
    }

    #[test]
    fn test_contains_blurb_requires_end_marker() {
        let content = "Some text\n<!-- br-agent-instructions-v1 -->\nblurb";

        assert!(!contains_blurb(content));
        assert_eq!(get_blurb_version(content), 0);
    }

    #[test]
    fn test_contains_blurb_skips_incomplete_marker_before_valid_block() {
        let content =
            format!("Example start marker: <!-- br-agent-instructions-v9 -->\n\n{AGENT_BLURB}");

        assert!(contains_blurb(&content));
        assert_eq!(get_blurb_version(&content), 1);
    }

    #[test]
    fn test_contains_legacy_blurb() {
        let content = "Some text\n<!-- bv-agent-instructions-v1 -->\nblurb\n<!-- end-bv-agent-instructions -->";
        assert!(!contains_blurb(content));
        assert!(contains_legacy_blurb(content));
        assert!(contains_any_blurb(content));
    }

    #[test]
    fn test_contains_legacy_blurb_requires_end_marker() {
        let content = "Some text\n<!-- bv-agent-instructions-v1 -->\nblurb";

        assert!(!contains_legacy_blurb(content));
    }

    #[test]
    fn test_get_blurb_version() {
        assert_eq!(
            get_blurb_version(
                "<!-- br-agent-instructions-v1 -->\n<!-- end-br-agent-instructions -->"
            ),
            1
        );
        assert_eq!(
            get_blurb_version(
                "<!-- br-agent-instructions-v2 -->\n<!-- end-br-agent-instructions -->"
            ),
            2
        );
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
    fn test_check_agent_file_reads_unreadable_path_once() {
        let temp_dir = TempDir::new().unwrap();
        let agents_path = temp_dir.path().join("AGENTS.md");
        fs::write(&agents_path, "# Agents\n").unwrap();

        let read_count = std::cell::Cell::new(0_usize);
        let detection = check_agent_file_with_reader(&agents_path, "AGENTS.md", |_| {
            read_count.set(read_count.get() + 1);
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "permission denied",
            ))
        })
        .unwrap();

        assert_eq!(read_count.get(), 1);
        assert!(detection.found());
        assert_eq!(detection.file_type.as_deref(), Some("AGENTS.md"));
        assert_eq!(detection.read_error.as_deref(), Some("permission denied"));
        assert!(detection.content.is_none());
    }

    #[test]
    fn test_unreadable_agent_file_does_not_need_blurb() {
        let detection = AgentFileDetection {
            file_path: Some(PathBuf::from("AGENTS.md")),
            file_type: Some("AGENTS.md".to_string()),
            read_error: Some("permission denied".to_string()),
            ..Default::default()
        };

        assert!(detection.found());
        assert!(detection.unreadable());
        assert!(!detection.needs_blurb());
    }

    #[test]
    fn test_inferred_dry_run_action_for_unreadable_agent_file_is_none() {
        let detection = AgentFileDetection {
            file_path: Some(PathBuf::from("AGENTS.md")),
            file_type: Some("AGENTS.md".to_string()),
            read_error: Some("permission denied".to_string()),
            ..Default::default()
        };

        assert_eq!(inferred_dry_run_action(&detection), "none");
    }

    #[test]
    fn test_execute_check_reports_inconsistent_detection_metadata() {
        let detection = AgentFileDetection {
            file_path: Some(PathBuf::from("AGENTS.md")),
            ..Default::default()
        };

        for mode in [OutputMode::Plain, OutputMode::Rich] {
            let ctx = OutputContext::with_mode(mode);
            let err = execute_check(&detection, Path::new("."), &ctx)
                .expect_err("missing file_type should be an internal error");

            assert!(matches!(
                err,
                BeadsError::Internal { message } if message.contains("missing file_type")
            ));
        }
    }

    #[test]
    fn test_execute_update_reports_missing_detection_content() {
        let detection = AgentFileDetection {
            file_path: Some(PathBuf::from("AGENTS.md")),
            file_type: Some("AGENTS.md".to_string()),
            has_blurb: true,
            blurb_version: 0,
            ..Default::default()
        };
        let ctx = OutputContext::from_flags(false, false, true);

        let err = execute_update(&detection, Path::new("."), true, true, &ctx)
            .expect_err("missing content should be an internal error");

        assert!(matches!(
            err,
            BeadsError::Internal { message } if message.contains("missing content")
        ));
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
    fn test_append_blurb_to_empty_content_has_no_leading_blank_lines() {
        let result = append_blurb("");

        assert!(result.starts_with(BLURB_START_MARKER));
        assert_eq!(result, format!("{AGENT_BLURB}\n"));
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
    fn test_remove_blurb_ignores_earlier_end_marker_text() {
        let content = format!(
            "# Agents\n\nLiteral marker: {BLURB_END_MARKER}\n\n{AGENT_BLURB}\n\nMore content."
        );

        let result = remove_blurb(&content);

        assert_eq!(
            result,
            format!("# Agents\n\nLiteral marker: {BLURB_END_MARKER}\n\nMore content.")
        );
    }

    #[test]
    fn test_remove_blurb_skips_incomplete_marker_before_valid_block() {
        let content = format!(
            "# Agents\n\nBroken example: <!-- br-agent-instructions-v9 -->\n\n{AGENT_BLURB}\n\nMore content."
        );

        let result = remove_blurb(&content);

        assert_eq!(
            result,
            "# Agents\n\nBroken example: <!-- br-agent-instructions-v9 -->\n\nMore content."
        );
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
    fn test_remove_legacy_blurb_ignores_earlier_end_marker_text() {
        let legacy_blurb =
            "<!-- bv-agent-instructions-v1 -->\nold\n<!-- end-bv-agent-instructions -->";
        let content = format!(
            "# Agents\n\nLiteral marker: <!-- end-bv-agent-instructions -->\n\n{legacy_blurb}\n\nMore content."
        );

        let result = remove_legacy_blurb(&content);

        assert_eq!(
            result,
            "# Agents\n\nLiteral marker: <!-- end-bv-agent-instructions -->\n\nMore content."
        );
    }

    #[test]
    fn test_remove_legacy_blurb_skips_incomplete_marker_before_valid_block() {
        let legacy_blurb =
            "<!-- bv-agent-instructions-v1 -->\nold\n<!-- end-bv-agent-instructions -->";
        let content = format!(
            "# Agents\n\nBroken example: <!-- bv-agent-instructions-v9 -->\n\n{legacy_blurb}\n\nMore content."
        );

        let result = remove_legacy_blurb(&content);

        assert_eq!(
            result,
            "# Agents\n\nBroken example: <!-- bv-agent-instructions-v9 -->\n\nMore content."
        );
    }

    #[test]
    fn test_remove_all_agent_blurbs_removes_legacy_and_current_blocks() {
        let legacy_blurb =
            "<!-- bv-agent-instructions-v1 -->\nold\n<!-- end-bv-agent-instructions -->";
        let content = format!("# Agents\n\n{legacy_blurb}\n\n{AGENT_BLURB}\n\nMore content.");

        let result = remove_all_agent_blurbs(&content);

        assert_eq!(result, "# Agents\n\nMore content.");
        assert!(!contains_any_blurb(&result));
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

    #[test]
    fn test_detect_in_deep_project_parents() {
        let temp_dir = TempDir::new().unwrap();
        let deep_dir = temp_dir
            .path()
            .join("a")
            .join("b")
            .join("c")
            .join("d")
            .join("e");
        fs::create_dir_all(&deep_dir).unwrap();
        fs::create_dir(temp_dir.path().join(".git")).unwrap();

        let agents_path = temp_dir.path().join("AGENTS.md");
        fs::write(&agents_path, "# Agents\n").unwrap();

        let detection = detect_agent_file_in_project(&deep_dir);
        assert!(detection.found());
        assert_eq!(detection.file_path.unwrap(), agents_path);
    }

    #[test]
    fn test_execute_json_add_force_creates_file() {
        let temp_dir = isolated_agent_project();
        let ctx = OutputContext::from_flags(true, false, true);

        execute_from(
            &AgentsArgs {
                add: true,
                force: true,
                ..Default::default()
            },
            &ctx,
            temp_dir.path(),
        )
        .expect("execute add in json mode");

        let agents_path = temp_dir.path().join("AGENTS.md");
        let content = fs::read_to_string(&agents_path).expect("read AGENTS.md");
        assert!(content.contains(BLURB_START_MARKER));
        assert!(content.contains(BLURB_END_MARKER));
        assert!(content.starts_with(BLURB_START_MARKER));
    }

    #[test]
    fn test_execute_remove_strips_legacy_and_current_blurbs() {
        let temp_dir = isolated_agent_project();
        let legacy_blurb =
            "<!-- bv-agent-instructions-v1 -->\nold\n<!-- end-bv-agent-instructions -->";
        let agents_path = temp_dir.path().join("AGENTS.md");
        fs::write(
            &agents_path,
            format!("# Agents\n\n{legacy_blurb}\n\n{AGENT_BLURB}\n\nMore content."),
        )
        .expect("write AGENTS.md with both blurb formats");
        let ctx = OutputContext::from_flags(true, false, true);

        execute_from(
            &AgentsArgs {
                remove: true,
                force: true,
                ..Default::default()
            },
            &ctx,
            temp_dir.path(),
        )
        .expect("remove both agent blurb formats");

        let content = fs::read_to_string(&agents_path).expect("read AGENTS.md after remove");
        assert_eq!(content, "# Agents\n\nMore content.");
        assert!(!contains_any_blurb(&content));
    }

    #[cfg(unix)]
    #[test]
    fn test_detect_agent_file_refuses_symlink() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();
        let outside_target = temp_dir.path().join("outside-agents.md");
        fs::write(&outside_target, "# Outside\n").unwrap();
        let agents_path = temp_dir.path().join("AGENTS.md");
        symlink(&outside_target, &agents_path).unwrap();

        let detection = detect_agent_file(temp_dir.path());

        assert!(detection.found());
        assert!(detection.unreadable());
        assert!(
            detection
                .read_error
                .as_deref()
                .is_some_and(|message| message.contains("symbolic links")),
            "symlinked AGENTS.md should be reported as unsupported"
        );
        assert!(detection.content.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn test_write_agent_file_atomically_rejects_symlink_target() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();
        let outside_target = temp_dir.path().join("outside-agents.md");
        fs::write(&outside_target, "# Outside\n").unwrap();
        let agents_path = temp_dir.path().join("AGENTS.md");
        symlink(&outside_target, &agents_path).unwrap();

        let err = write_agent_file_atomically(&agents_path, b"# New\n")
            .expect_err("atomic rewrite should reject symlink targets");

        assert!(
            format!("{err}").contains("must not be a symlink"),
            "error should explain that symlinked rewrite targets are unsupported"
        );
        assert_eq!(
            fs::read_to_string(&outside_target).unwrap(),
            "# Outside\n",
            "rewrite must not modify the symlink target"
        );
        assert!(
            fs::symlink_metadata(&agents_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "rejected rewrite target symlink should be left untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_backup_agent_file_refuses_symlinked_backup_path() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();
        let agents_path = temp_dir.path().join("AGENTS.md");
        fs::write(&agents_path, "# Agents\n").unwrap();
        let outside_backup_target = temp_dir.path().join("outside-backup.md");
        fs::write(&outside_backup_target, "# Outside backup\n").unwrap();
        let backup_path = agents_path.with_extension("md.bak");
        symlink(&outside_backup_target, &backup_path).unwrap();
        let ctx = OutputContext::from_flags(true, false, true);

        let backup = backup_agent_file(&agents_path, &ctx);

        assert!(
            backup.is_none(),
            "backup creation should fail when the backup path is a symlink"
        );
        assert_eq!(
            fs::read_to_string(&outside_backup_target).unwrap(),
            "# Outside backup\n",
            "backup creation must not modify the symlink target"
        );
        assert!(
            fs::symlink_metadata(&backup_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "rejected backup path symlink should be left untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_execute_json_add_refuses_dangling_agents_symlink() {
        use std::os::unix::fs::symlink;

        let temp_dir = isolated_agent_project();
        let outside_target = temp_dir.path().join("outside-agents.md");
        let agents_path = temp_dir.path().join("AGENTS.md");
        symlink(&outside_target, &agents_path).unwrap();
        let ctx = OutputContext::from_flags(true, false, true);

        let err = execute_from(
            &AgentsArgs {
                add: true,
                force: true,
                ..Default::default()
            },
            &ctx,
            temp_dir.path(),
        )
        .expect_err("json add should reject symlinked AGENTS.md");

        assert!(
            format!("{err}").contains("symbolic links"),
            "error should explain that symlinked agent files are unsupported"
        );
        assert!(
            !outside_target.exists(),
            "br agents --add must not create a dangling symlink target"
        );
        assert!(
            fs::symlink_metadata(&agents_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "rejected AGENTS.md symlink should be left untouched"
        );
    }

    #[test]
    fn test_execute_json_add_requires_force() {
        let temp_dir = isolated_agent_project();
        let ctx = OutputContext::from_flags(true, false, true);

        let err = execute_from(
            &AgentsArgs {
                add: true,
                ..Default::default()
            },
            &ctx,
            temp_dir.path(),
        )
        .expect_err("json add without force should fail");

        match err {
            BeadsError::Validation { field, reason } => {
                assert_eq!(field, "force");
                assert!(reason.contains("--force"));
            }
            other => assert_unexpected_error(&other),
        }
    }

    #[test]
    fn test_execute_json_add_dry_run_does_not_require_force() {
        let temp_dir = isolated_agent_project();
        let ctx = OutputContext::from_flags(true, false, true);

        execute_from(
            &AgentsArgs {
                add: true,
                dry_run: true,
                ..Default::default()
            },
            &ctx,
            temp_dir.path(),
        )
        .expect("json add dry-run should succeed without force");

        assert!(
            !temp_dir.path().join("AGENTS.md").exists(),
            "dry-run must not create AGENTS.md"
        );
    }

    #[test]
    fn test_execute_json_remove_dry_run_without_file_errors() {
        let temp_dir = isolated_agent_project();
        let ctx = OutputContext::from_flags(true, false, true);

        let err = execute_from(
            &AgentsArgs {
                remove: true,
                dry_run: true,
                ..Default::default()
            },
            &ctx,
            temp_dir.path(),
        )
        .expect_err("json remove dry-run without file should error");

        match err {
            BeadsError::Validation { field, reason } => {
                assert_eq!(field, "AGENTS.md");
                assert!(reason.contains("current directory"));
            }
            other => assert_unexpected_error(&other),
        }
    }

    #[test]
    fn test_execute_json_update_dry_run_without_file_errors() {
        let temp_dir = isolated_agent_project();
        let ctx = OutputContext::from_flags(true, false, true);

        let err = execute_from(
            &AgentsArgs {
                update: true,
                dry_run: true,
                ..Default::default()
            },
            &ctx,
            temp_dir.path(),
        )
        .expect_err("json update dry-run without file should error");

        match err {
            BeadsError::Validation { field, reason } => {
                assert_eq!(field, "AGENTS.md");
                assert!(reason.contains("current directory"));
            }
            other => assert_unexpected_error(&other),
        }
    }

    #[test]
    fn test_search_scope_description_for_project_subdir() {
        let temp_dir = TempDir::new().unwrap();
        fs::create_dir(temp_dir.path().join(".git")).unwrap();
        let sub_dir = temp_dir.path().join("nested").join("deeper");
        fs::create_dir_all(&sub_dir).unwrap();

        let description = search_scope_description(&sub_dir);

        assert!(description.contains(&sub_dir.display().to_string()));
        assert!(description.contains(&temp_dir.path().display().to_string()));
        assert!(description.contains("project root"));
    }

    #[test]
    fn test_agent_file_not_found_reason_for_project_subdir() {
        let temp_dir = TempDir::new().unwrap();
        fs::create_dir(temp_dir.path().join(".git")).unwrap();
        let sub_dir = temp_dir.path().join("nested");
        fs::create_dir(&sub_dir).unwrap();

        let reason = agent_file_not_found_reason(&sub_dir);

        assert!(reason.contains("not found between current directory"));
        assert!(reason.contains(&sub_dir.display().to_string()));
        assert!(reason.contains(&temp_dir.path().display().to_string()));
    }

    #[test]
    fn test_add_confirmation_message_for_new_file() {
        let file_path = PathBuf::from("/tmp/AGENTS.md");
        let message = add_confirmation_message(&AgentFileDetection::default(), &file_path);

        assert!(message.contains("create a new AGENTS.md"));
        assert!(message.contains(file_path.to_string_lossy().as_ref()));
    }

    #[test]
    fn test_add_confirmation_message_for_existing_file() {
        let detection = AgentFileDetection {
            file_path: Some(PathBuf::from("/tmp/CLAUDE.md")),
            file_type: Some("CLAUDE.md".to_string()),
            content: Some("# Existing".to_string()),
            ..Default::default()
        };
        let file_path = detection.file_path.clone().unwrap();
        let message = add_confirmation_message(&detection, &file_path);

        assert!(message.contains("add beads workflow instructions to"));
        assert!(message.contains(file_path.to_string_lossy().as_ref()));
        assert!(!message.contains("create a new AGENTS.md"));
    }

    #[test]
    fn test_execute_rejects_conflicting_actions() {
        let ctx = OutputContext::from_flags(false, false, true);

        let err = execute(
            &AgentsArgs {
                add: true,
                check: true,
                ..Default::default()
            },
            &ctx,
        )
        .expect_err("conflicting actions should fail");

        match err {
            BeadsError::Validation { field, reason } => {
                assert_eq!(field, "action");
                assert!(reason.contains("--add"));
                assert!(reason.contains("--check"));
            }
            other => assert_unexpected_error(&other),
        }
    }
}
