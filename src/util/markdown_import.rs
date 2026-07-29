//! Markdown bulk import parser for `br create --file`.
//!
//! Parses a markdown file with a specific grammar to create multiple issues.
//!
//! # Markdown Grammar
//!
//! - Each issue starts with an H2 line: `## Issue Title`
//! - Per-issue sections are H3 lines: `### Section Name`
//! - Recognized sections (case-insensitive):
//!   - ID, Priority, Type, Description, Design, Acceptance Criteria (alias Acceptance),
//!     Assignee, Labels, Dependencies (alias Deps), Agent Context (alias Agent-Context)
//! - Unknown sections are ignored
//!
//! # Intra-file Dependency References
//!
//! Dependencies can reference other issues in the same import file by:
//! - **Title**: use the exact H2 title text (e.g., `Build Database Schema`)
//! - **Stand-in ID**: assign `### ID` to an issue, then reference that ID from
//!   other issues' `### Dependencies` section (e.g., `db-1`)
//!
//! These symbolic references are resolved to real generated IDs during import.
//! References to pre-existing issues in storage still work via normal ID resolution.
//!
//! # Known Quirk (matches bd behavior)
//!
//! Lines immediately after the H2 title before any H3 are treated as description,
//! but **only the first non-empty line** is captured; subsequent lines are ignored.

use crate::error::{BeadsError, Result};
use crate::model::DependencyType;
use std::fs;
use std::io::Read;
use std::path::{Component, Path};
use std::str::FromStr;

const MAX_MARKDOWN_IMPORT_BYTES: usize = 10 * 1024 * 1024;
const MAX_MARKDOWN_IMPORT_BYTES_U64: u64 = 10 * 1024 * 1024;

/// A parsed issue from the markdown file.
#[derive(Debug, Default, Clone)]
pub struct ParsedIssue {
    /// Issue title from the H2 header.
    pub title: String,
    /// Optional stand-in ID for intra-file dependency references (e.g. "db-1").
    /// This ID is NOT used as the actual issue ID — it only serves as a symbolic
    /// handle so other issues in the same import file can reference this one.
    pub stand_in_id: Option<String>,
    /// Parent issue ID (e.g. "bd-123").
    pub parent: Option<String>,
    /// Priority string (e.g., "0", "P1", "2").
    pub priority: Option<String>,
    /// Issue type (e.g., "task", "bug", "feature").
    pub issue_type: Option<String>,
    /// Description content.
    pub description: Option<String>,
    /// Design section content.
    pub design: Option<String>,
    /// Acceptance criteria content.
    pub acceptance_criteria: Option<String>,
    /// Assignee name.
    pub assignee: Option<String>,
    /// Labels list.
    pub labels: Vec<String>,
    /// Dependencies list (format: "type:id" or "id").
    pub dependencies: Vec<String>,
    /// Agent-context governance text (opaque, same semantics as
    /// `br update --agent-context`). beads_rust#304.
    pub agent_context: Option<String>,
}

/// Section types recognized in the markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    /// Before any H3, capturing implicit description
    BeforeH3,
    Id,
    Parent,
    Priority,
    Type,
    Description,
    Design,
    AcceptanceCriteria,
    Assignee,
    Labels,
    Dependencies,
    AgentContext,
    Unknown,
}

impl Section {
    fn from_header(header: &str) -> Self {
        let normalized = header.trim().to_lowercase();
        match normalized.as_str() {
            "id" => Self::Id,
            "parent" => Self::Parent,
            "priority" => Self::Priority,
            "type" => Self::Type,
            "description" => Self::Description,
            "design" => Self::Design,
            "acceptance criteria" | "acceptance" => Self::AcceptanceCriteria,
            "assignee" => Self::Assignee,
            "labels" => Self::Labels,
            "dependencies" | "deps" => Self::Dependencies,
            "agent context" | "agent-context" | "agent_context" => Self::AgentContext,
            _ => Self::Unknown,
        }
    }
}

/// Parse a markdown file into a list of issues.
///
/// # Arguments
///
/// * `path` - Path to the markdown file (must be .md or .markdown)
///
/// # Errors
///
/// Returns an error if:
/// - The file doesn't exist
/// - The file extension is not .md or .markdown
/// - The path contains ".." (path traversal)
/// - The path is a symlink or not a regular file
/// - The file cannot be read
pub fn parse_markdown_file(path: &Path) -> Result<Vec<ParsedIssue>> {
    // Validate file extension
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);

    match extension.as_deref() {
        Some("md" | "markdown") => {}
        _ => {
            return Err(BeadsError::validation(
                "file",
                "must have .md or .markdown extension",
            ));
        }
    }

    // Reject path traversal segments to match classic bd behavior.
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(BeadsError::validation("file", "path must not contain '..'"));
    }

    // Check file exists
    if !path.exists() {
        return Err(BeadsError::validation(
            "file",
            format!("file not found: {}", path.display()),
        ));
    }

    let metadata = fs::symlink_metadata(path)
        .map_err(|e| BeadsError::validation("file", format!("cannot inspect file: {e}")))?;
    if metadata.file_type().is_symlink() {
        return Err(BeadsError::validation(
            "file",
            "symlinked markdown imports are not allowed",
        ));
    }
    if !metadata.is_file() {
        return Err(BeadsError::validation("file", "must be a regular file"));
    }

    let content = read_markdown_file_limited(path, &metadata)?;

    parse_markdown_content(&content)
}

fn read_markdown_file_limited(path: &Path, metadata: &fs::Metadata) -> Result<String> {
    if metadata.len() > MAX_MARKDOWN_IMPORT_BYTES_U64 {
        return Err(BeadsError::validation(
            "file",
            format!("markdown import exceeds maximum size of {MAX_MARKDOWN_IMPORT_BYTES} bytes"),
        ));
    }

    let file = fs::File::open(path)
        .map_err(|e| BeadsError::validation("file", format!("cannot read file: {e}")))?;
    let mut reader = file.take(MAX_MARKDOWN_IMPORT_BYTES_U64.saturating_add(1));
    let mut content = Vec::new();
    reader
        .read_to_end(&mut content)
        .map_err(|e| BeadsError::validation("file", format!("cannot read file: {e}")))?;
    if content.len() > MAX_MARKDOWN_IMPORT_BYTES {
        return Err(BeadsError::validation(
            "file",
            format!("markdown import exceeds maximum size of {MAX_MARKDOWN_IMPORT_BYTES} bytes"),
        ));
    }

    String::from_utf8(content)
        .map_err(|e| BeadsError::validation("file", format!("markdown must be valid UTF-8: {e}")))
}

/// Parse markdown content string into a list of issues.
///
/// This is the core parsing logic, separated for testability.
///
/// # Errors
///
/// Returns an error if the content cannot be parsed into issues.
pub fn parse_markdown_content(content: &str) -> Result<Vec<ParsedIssue>> {
    let has_non_whitespace_content = content.lines().any(|line| !line.trim().is_empty());
    let mut issues = Vec::new();
    let mut current_issue: Option<ParsedIssue> = None;
    let mut current_section = Section::BeforeH3;
    let mut section_lines: Vec<String> = Vec::new();
    let mut captured_implicit_desc = false;

    for line in content.lines() {
        // Check for H2 (new issue)
        if let Some(stripped) = line.strip_prefix("## ") {
            // Save previous issue
            if let Some(mut issue) = current_issue.take() {
                apply_section_to_issue(&mut issue, current_section, &section_lines);
                issues.push(issue);
            }

            // Start new issue
            let title = stripped.trim().to_string();
            current_issue = Some(ParsedIssue {
                title,
                ..Default::default()
            });
            current_section = Section::BeforeH3;
            section_lines.clear();
            captured_implicit_desc = false;
            continue;
        }

        // Check for H3 (section header)
        if let Some(stripped) = line.strip_prefix("### ") {
            if let Some(ref mut issue) = current_issue {
                // Apply previous section
                apply_section_to_issue(issue, current_section, &section_lines);

                // Start new section
                let header = stripped.trim();
                current_section = Section::from_header(header);
                section_lines.clear();
            }
            continue;
        }

        // Collect content for current section
        if current_issue.is_some() {
            if current_section == Section::BeforeH3 {
                if !captured_implicit_desc && !line.trim().is_empty() {
                    section_lines.push(line.to_string());
                    captured_implicit_desc = true;
                }
            } else {
                section_lines.push(line.to_string());
            }
        }
    }

    // Don't forget the last issue
    if let Some(mut issue) = current_issue {
        apply_section_to_issue(&mut issue, current_section, &section_lines);
        issues.push(issue);
    }

    if issues.is_empty() && has_non_whitespace_content {
        return Err(BeadsError::validation(
            "file",
            "no issues found; expected '## Title' headers",
        ));
    }

    Ok(issues)
}

/// Apply collected section content to an issue.
fn apply_section_to_issue(issue: &mut ParsedIssue, section: Section, lines: &[String]) {
    let content = lines.join("\n").trim().to_string();

    if content.is_empty() {
        return;
    }

    match section {
        Section::BeforeH3 => {
            // Implicit description (first non-empty line only)
            if issue.description.is_none() {
                issue.description = Some(content);
            }
        }
        Section::Id => {
            issue.stand_in_id = Some(content);
        }
        Section::Parent => {
            issue.parent = Some(content);
        }
        Section::Priority => {
            issue.priority = Some(content);
        }
        Section::Type => {
            issue.issue_type = Some(content);
        }
        Section::Description => {
            issue.description = Some(content);
        }
        Section::Design => {
            issue.design = Some(content);
        }
        Section::AcceptanceCriteria => {
            issue.acceptance_criteria = Some(content);
        }
        Section::Assignee => {
            issue.assignee = Some(content);
        }
        Section::Labels => {
            issue.labels = split_list_content(&content);
        }
        Section::Dependencies => {
            issue.dependencies = split_dependency_content(&content);
        }
        Section::AgentContext => {
            // Opaque text, same semantics as `br update --agent-context`.
            issue.agent_context = Some(content);
        }
        Section::Unknown => {
            // Ignore unknown sections
        }
    }
}

/// Split dependency content, preserving bulleted lines as whole items.
///
/// Bulleted lines (`- `, `* `, `+ `) are treated as single dependency references
/// to support title-based and multi-word stand-in ID references. Non-bulleted
/// lines are split on commas or whitespace (preserving `type:id` pairs).
fn split_dependency_content(content: &str) -> Vec<String> {
    let mut result = Vec::new();
    for raw_line in content.lines() {
        let trimmed = raw_line.trim_start();
        let is_bulleted =
            trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ");
        let line = strip_markdown_list_prefix(raw_line).trim();
        if line.is_empty() || is_marker_only_token(line) {
            continue;
        }
        if is_bulleted {
            // Treat the whole stripped line as a single dependency reference.
            // This allows title-based refs like "- Build Database Schema".
            // Note: `line` already has bullets and checkboxes stripped via
            // `strip_markdown_list_prefix`, and emptiness was checked above.
            result.push(line.to_string());
        } else if line.contains(',') {
            result.extend(
                line.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty() && !is_marker_only_token(s)),
            );
        } else {
            result.extend(split_whitespace_items_preserving_colon_pairs(line));
        }
    }
    result
}

/// Split content on commas or whitespace for labels/deps.
fn split_list_content(content: &str) -> Vec<String> {
    let mut result = Vec::new();
    for raw_line in content.lines() {
        let line = strip_markdown_list_prefix(raw_line).trim();
        if line.is_empty() || is_marker_only_token(line) {
            continue;
        }
        if line.contains(',') {
            result.extend(
                line.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty() && !is_marker_only_token(s)),
            );
        } else {
            result.extend(split_whitespace_items_preserving_colon_pairs(line));
        }
    }
    result
}

fn split_whitespace_items_preserving_colon_pairs(line: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut pending_colon_prefix: Option<String> = None;

    for token in line
        .split_whitespace()
        .filter(|token| !token.is_empty() && !is_marker_only_token(token))
    {
        if let Some(mut prefix) = pending_colon_prefix.take() {
            prefix.push(' ');
            prefix.push_str(token);
            items.push(prefix);
            continue;
        }

        if token.ends_with(':') && token != ":" {
            pending_colon_prefix = Some(token.to_string());
        } else {
            items.push(token.to_string());
        }
    }

    if let Some(prefix) = pending_colon_prefix {
        items.push(prefix);
    }

    items
}

fn strip_markdown_list_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();

    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return strip_markdown_checkbox_prefix(rest);
        }
    }

    strip_markdown_checkbox_prefix(trimmed)
}

fn strip_markdown_checkbox_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();

    for marker in ["[ ] ", "[x] ", "[X] "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return rest;
        }
    }

    trimmed
}

fn is_marker_only_token(token: &str) -> bool {
    matches!(token.trim(), "-" | "*" | "+")
}

/// Validate a dependency type string.
///
/// Returns the dependency type if valid, or None if invalid.
#[must_use]
pub fn validate_dependency_type(dep_type: &str) -> Option<&str> {
    // Check against standard types
    if let Ok(dt) = DependencyType::from_str(dep_type) {
        if let DependencyType::Custom(_) = dt {
            // Check for legacy/alias support not in standard enum
            if dep_type.eq_ignore_ascii_case("blocked-by") {
                return Some(dep_type);
            }
            return None;
        }
        return Some(dep_type);
    }
    None
}

/// Parse a dependency string into (type, id).
///
/// Accepts "type:id" or bare "id" (defaults to "blocks").
///
/// Returns (`dep_type`, `dep_id`, `is_valid_type`) where `is_valid_type` indicates
/// whether the type was recognized.
#[must_use]
pub fn parse_dependency(dep_str: &str) -> (String, String, bool) {
    if dep_str.starts_with("external:") {
        ("blocks".to_string(), dep_str.to_string(), true)
    } else if let Some((type_part, id_part)) = dep_str.split_once(':') {
        let type_part = type_part.trim();
        let id_part = id_part.trim();
        if validate_dependency_type(type_part).is_some() {
            (type_part.to_string(), id_part.to_string(), true)
        } else {
            // Type part is not a valid dependency type, so the colon is likely part of the title/ID.
            // Treat the whole string as the ID with default 'blocks' type.
            ("blocks".to_string(), dep_str.trim().to_string(), true)
        }
    } else {
        ("blocks".to_string(), dep_str.to_string(), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::BeadsError;
    use std::path::Path;

    #[test]
    fn test_parse_simple_issue() {
        let content = r"## My First Issue
### Parent
proj-abc123

### Description
This is the description.

### Priority
1

### Type
bug
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "My First Issue");
        assert_eq!(issues[0].parent, Some("proj-abc123".to_string()));
        assert_eq!(
            issues[0].description,
            Some("This is the description.".to_string())
        );
        assert_eq!(issues[0].priority, Some("1".to_string()));
        assert_eq!(issues[0].issue_type, Some("bug".to_string()));
    }

    #[test]
    fn test_parse_multiple_issues() {
        let content = r"## Issue One
### Type
task

## Issue Two
### Type
feature

## Issue Three
### Type
bug
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues.len(), 3);
        assert_eq!(issues[0].title, "Issue One");
        assert_eq!(issues[1].title, "Issue Two");
        assert_eq!(issues[2].title, "Issue Three");
    }

    #[test]
    fn test_implicit_description_quirk() {
        // Only first non-empty line before H3 is captured
        let content = r"## Issue Title
First line becomes description
This line is ignored
And this one too

### Priority
2
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(
            issues[0].description,
            Some("First line becomes description".to_string())
        );
    }

    #[test]
    fn test_labels_comma_separated() {
        let content = r"## Test Issue
### Labels
bug, urgent, frontend
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues[0].labels, vec!["bug", "urgent", "frontend"]);
    }

    #[test]
    fn test_labels_whitespace_separated() {
        let content = r"## Test Issue
### Labels
bug urgent frontend
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues[0].labels, vec!["bug", "urgent", "frontend"]);
    }

    #[test]
    fn test_dependencies_parsing() {
        let content = r"## Test Issue
### Dependencies
blocks:bd-123, bd-456, related:bd-789
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(
            issues[0].dependencies,
            vec!["blocks:bd-123", "bd-456", "related:bd-789"]
        );
    }

    #[test]
    fn test_dependencies_markdown_bullets_ignore_list_markers() {
        let content = r"## Test Issue
### Dependencies
- bd-123
- [ ] related:bd-456
* external:github#123
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(
            issues[0].dependencies,
            vec!["bd-123", "related:bd-456", "external:github#123"]
        );
    }

    #[test]
    fn test_dependencies_whitespace_separated_typed_tokens() {
        let content = r"## Test Issue
### Dependencies
blocks: bd-123 related:bd-456 external:github#123
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(
            issues[0].dependencies,
            vec!["blocks: bd-123", "related:bd-456", "external:github#123"]
        );
    }

    #[test]
    fn test_acceptance_criteria_alias() {
        let content = r"## Test Issue
### Acceptance
- [ ] First criterion
- [ ] Second criterion
";
        let issues = parse_markdown_content(content).unwrap();
        assert!(issues[0].acceptance_criteria.is_some());
        assert!(
            issues[0]
                .acceptance_criteria
                .as_ref()
                .unwrap()
                .contains("First criterion")
        );
    }

    #[test]
    fn test_unknown_sections_ignored() {
        let content = r"## Test Issue
### Unknown Section
This content should be ignored.

### Description
This is the actual description.
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(
            issues[0].description,
            Some("This is the actual description.".to_string())
        );
    }

    #[test]
    fn test_validate_dependency_type() {
        assert!(validate_dependency_type("blocks").is_some());
        assert!(validate_dependency_type("blocked-by").is_some());
        assert!(validate_dependency_type("parent-child").is_some());
        assert!(validate_dependency_type("related").is_some());
        assert!(validate_dependency_type("duplicates").is_some());
        assert!(validate_dependency_type("invalid").is_none());
    }

    #[test]
    fn test_split_list_content_spaces() {
        let content = "- blocks: bd-123\n- parent-child: bd-456";
        let items = split_list_content(content);
        assert_eq!(items, vec!["blocks: bd-123", "parent-child: bd-456"]);
    }

    #[test]
    fn test_parse_dependency() {
        let (t, id, valid) = parse_dependency("blocks:bd-123");
        assert_eq!(t, "blocks");
        assert_eq!(id, "bd-123");
        assert!(valid);

        let (t, id, valid) = parse_dependency("blocks: bd-456 ");
        assert_eq!(t, "blocks");
        assert_eq!(id, "bd-456");
        assert!(valid);

        let (t, id, valid) = parse_dependency("bd-456");
        assert_eq!(t, "blocks");
        assert_eq!(id, "bd-456");
        assert!(valid);

        // Invalid type prefixes should now be treated as part of the ID (e.g. for title matches)
        let (t, id, valid) = parse_dependency("invalid:bd-789");
        assert_eq!(t, "blocks");
        assert_eq!(id, "invalid:bd-789");
        assert!(valid);
    }

    #[test]
    fn test_parse_markdown_file_rejects_parent_dir() {
        let err = parse_markdown_file(Path::new("../issues.md")).unwrap_err();
        assert!(matches!(err, BeadsError::Validation { field, .. } if field == "file"));
    }

    #[test]
    fn test_parse_markdown_file_rejects_directory() {
        let temp = tempfile::tempdir().unwrap();
        let err = parse_markdown_file(temp.path()).unwrap_err();

        assert!(
            matches!(err, BeadsError::Validation { field, reason } if field == "file" && reason.contains(".md"))
        );
    }

    #[test]
    fn test_parse_markdown_file_rejects_non_regular_md_path() {
        let temp = tempfile::tempdir().unwrap();
        let dir_path = temp.path().join("issues.md");
        fs::create_dir(&dir_path).unwrap();

        let err = parse_markdown_file(&dir_path).unwrap_err();

        assert!(
            matches!(err, BeadsError::Validation { field, reason } if field == "file" && reason.contains("regular file"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_markdown_file_rejects_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.md");
        let link = temp.path().join("issues.md");
        fs::write(&target, "## Imported\n").unwrap();
        symlink(&target, &link).unwrap();

        let err = parse_markdown_file(&link).unwrap_err();

        assert!(
            matches!(err, BeadsError::Validation { field, reason } if field == "file" && reason.contains("symlink"))
        );
    }

    #[test]
    fn test_parse_markdown_file_rejects_oversized_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("issues.md");
        fs::write(&path, "## Imported\n").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_MARKDOWN_IMPORT_BYTES_U64 + 1).unwrap();

        let err = parse_markdown_file(&path).unwrap_err();

        assert!(
            matches!(err, BeadsError::Validation { field, reason } if field == "file" && reason.contains("maximum size"))
        );
    }

    #[test]
    fn test_read_markdown_file_limited_checks_size_before_utf8_decode() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("issues.md");
        fs::write(&path, "## Imported\n").unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        let mut payload = vec![b'a'; MAX_MARKDOWN_IMPORT_BYTES];
        payload.push(0xc3);
        fs::write(&path, payload).unwrap();

        let err = read_markdown_file_limited(&path, &metadata).unwrap_err();

        assert!(
            matches!(err, BeadsError::Validation { field, reason } if field == "file" && reason.contains("maximum size"))
        );
    }

    #[test]
    fn test_parse_markdown_file_rejects_invalid_utf8() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("issues.md");
        fs::write(&path, [0xff]).unwrap();

        let err = parse_markdown_file(&path).unwrap_err();

        assert!(
            matches!(err, BeadsError::Validation { field, reason } if field == "file" && reason.contains("valid UTF-8"))
        );
    }

    #[test]
    fn test_parse_markdown_content_rejects_non_empty_content_without_issue_headers() {
        let err = parse_markdown_content("### Description\nNo issue header here.\n").unwrap_err();
        assert!(matches!(err, BeadsError::Validation { field, .. } if field == "file"));
    }

    #[test]
    fn test_stand_in_id_section() {
        let content = r"## Build Database Schema
### ID
db-1
### Type
task

## Build API Endpoints
### Type
feature
### Dependencies
db-1
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].stand_in_id, Some("db-1".to_string()));
        assert_eq!(issues[0].title, "Build Database Schema");
        assert_eq!(issues[1].dependencies, vec!["db-1"]);
    }

    #[test]
    fn test_title_based_dependencies_bulleted() {
        let content = r"## Build API Endpoints
### Type
feature
### Dependencies
- Build Database Schema

## Build Database Schema
### Type
task
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues.len(), 2);
        // Bulleted line is preserved as a single dependency reference
        assert_eq!(issues[0].dependencies, vec!["Build Database Schema"]);
    }

    #[test]
    fn test_non_bulleted_deps_still_split_on_whitespace() {
        let content = r"## Test Issue
### Dependencies
bd-123 bd-456
";
        let issues = parse_markdown_content(content).unwrap();
        // Non-bulleted, space-separated: split on whitespace (existing behavior)
        assert_eq!(issues[0].dependencies, vec!["bd-123", "bd-456"]);
    }

    #[test]
    fn test_design_section() {
        let content = r"## Test Issue
### Design
Design notes here.
Multi-line content.
";
        let issues = parse_markdown_content(content).unwrap();
        assert!(issues[0].design.is_some());
        assert!(issues[0].design.as_ref().unwrap().contains("Design notes"));
    }

    #[test]
    fn test_case_insensitive_sections() {
        let content = r"## Test Issue
### PRIORITY
1

### description
Test desc

### TYPE
task
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues[0].priority, Some("1".to_string()));
        assert_eq!(issues[0].description, Some("Test desc".to_string()));
        assert_eq!(issues[0].issue_type, Some("task".to_string()));
    }

    #[test]
    fn test_explicit_description_overrides_implicit() {
        let content = r"## Test Issue
Implicit description line

### Description
Explicit description content
";
        let issues = parse_markdown_content(content).unwrap();
        // Explicit ### Description section should be used
        assert_eq!(
            issues[0].description,
            Some("Explicit description content".to_string())
        );
    }

    #[test]
    fn test_agent_context_section_parsing() {
        let content = r"## Epic With Context
### Agent Context
Use the porting-to-rust skill. Constraints: no rusqlite; fsqlite only.
### Type
epic
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(
            issues[0].agent_context.as_deref(),
            Some("Use the porting-to-rust skill. Constraints: no rusqlite; fsqlite only.")
        );
    }

    #[test]
    fn test_agent_context_section_is_opaque_multiline() {
        let content = r#"## Issue
### Agent Context
{"skills": ["porting-to-rust"], "constraints": ["no rusqlite"]}
second line of opaque text
"#;
        let issues = parse_markdown_content(content).unwrap();
        let ctx = issues[0].agent_context.as_deref().unwrap();
        assert!(ctx.contains("porting-to-rust"));
        assert!(ctx.contains("second line of opaque text"));
    }

    #[test]
    fn test_agent_context_aliases() {
        for header in [
            "Agent Context",
            "agent-context",
            "AGENT CONTEXT",
            "agent_context",
        ] {
            let content = format!("## Issue\n### {header}\nopaque body\n");
            let issues = parse_markdown_content(&content).unwrap();
            assert_eq!(
                issues[0].agent_context.as_deref(),
                Some("opaque body"),
                "header `{header}` should map to agent_context"
            );
        }
    }

    #[test]
    fn test_agent_context_absent_when_section_missing() {
        let content = r"## Issue
### Description
just a description
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues[0].agent_context, None);
        assert_eq!(issues[0].description.as_deref(), Some("just a description"));
    }

    #[test]
    fn test_unknown_h3_section_still_ignored_alongside_agent_context() {
        let content = r"## Issue
### Totally Unknown Section
ignored content
### Agent Context
kept content
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues[0].agent_context.as_deref(), Some("kept content"));
        // The unknown section did not leak into any recognized field.
        assert_eq!(issues[0].description, None);
        assert_eq!(issues[0].design, None);
    }

    #[test]
    fn test_parent_section_parsing() {
        let content = r"## Test Issue
### Parent
bd-123
";
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues[0].parent, Some("bd-123".to_string()));
    }
}
