//! Markdown bulk import parser for `br create --file`.
//!
//! Parses a markdown file with a specific grammar to create multiple issues.
//!
//! # Markdown Grammar
//!
//! - Each issue starts with an H2 line: `## Issue Title`
//! - Per-issue sections are H3 lines: `### Section Name`
//! - Recognized sections (case-insensitive):
//!   - Priority, Type, Description, Design, Acceptance Criteria (alias Acceptance),
//!     Assignee, Labels, Dependencies (alias Deps)
//! - Unknown sections are ignored
//!
//! # Known Quirk (matches bd behavior)
//!
//! Lines immediately after the H2 title before any H3 are treated as description,
//! but **only the first non-empty line** is captured; subsequent lines are ignored.

use crate::error::{BeadsError, Result};
use std::fs;
use std::path::Path;

/// A parsed issue from the markdown file.
#[derive(Debug, Default, Clone)]
pub struct ParsedIssue {
    /// Issue title from the H2 header.
    pub title: String,
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
}

/// Section types recognized in the markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    /// Before any H3, capturing implicit description
    BeforeH3,
    Priority,
    Type,
    Description,
    Design,
    AcceptanceCriteria,
    Assignee,
    Labels,
    Dependencies,
    Unknown,
}

impl Section {
    fn from_header(header: &str) -> Self {
        let normalized = header.trim().to_lowercase();
        match normalized.as_str() {
            "priority" => Self::Priority,
            "type" => Self::Type,
            "description" => Self::Description,
            "design" => Self::Design,
            "acceptance criteria" | "acceptance" => Self::AcceptanceCriteria,
            "assignee" => Self::Assignee,
            "labels" => Self::Labels,
            "dependencies" | "deps" => Self::Dependencies,
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
/// - The file cannot be read
pub fn parse_markdown_file(path: &Path) -> Result<Vec<ParsedIssue>> {
    // Validate file extension
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());

    match extension.as_deref() {
        Some("md") | Some("markdown") => {}
        _ => {
            return Err(BeadsError::validation(
                "file",
                "must have .md or .markdown extension",
            ));
        }
    }

    // Check for path traversal
    let path_str = path.to_string_lossy();
    if path_str.contains("..") {
        return Err(BeadsError::validation("file", "path must not contain '..'"));
    }

    // Check file exists
    if !path.exists() {
        return Err(BeadsError::validation(
            "file",
            format!("file not found: {}", path.display()),
        ));
    }

    // Read file content
    let content = fs::read_to_string(path)
        .map_err(|e| BeadsError::validation("file", format!("cannot read file: {e}")))?;

    parse_markdown_content(&content)
}

/// Parse markdown content string into a list of issues.
///
/// This is the core parsing logic, separated for testability.
pub fn parse_markdown_content(content: &str) -> Result<Vec<ParsedIssue>> {
    let mut issues = Vec::new();
    let mut current_issue: Option<ParsedIssue> = None;
    let mut current_section = Section::BeforeH3;
    let mut section_lines: Vec<String> = Vec::new();
    let mut captured_implicit_desc = false;

    for line in content.lines() {
        // Check for H2 (new issue)
        if line.starts_with("## ") && !line.starts_with("### ") {
            // Save previous issue
            if let Some(mut issue) = current_issue.take() {
                apply_section_to_issue(&mut issue, current_section, &section_lines);
                issues.push(issue);
            }

            // Start new issue
            let title = line[3..].trim().to_string();
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
        if line.starts_with("### ") {
            if let Some(ref mut issue) = current_issue {
                // Apply previous section
                apply_section_to_issue(issue, current_section, &section_lines);

                // Start new section
                let header = line[4..].trim();
                current_section = Section::from_header(header);
                section_lines.clear();
            }
            continue;
        }

        // Collect content for current section
        if current_issue.is_some() {
            // Handle the quirk: before H3, only capture first non-empty line as description
            if current_section == Section::BeforeH3 {
                if !captured_implicit_desc && !line.trim().is_empty() {
                    section_lines.push(line.to_string());
                    captured_implicit_desc = true;
                }
                // Ignore subsequent lines before H3
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

    Ok(issues)
}

/// Apply collected section content to an issue.
fn apply_section_to_issue(issue: &mut ParsedIssue, section: Section, lines: &[String]) {
    let content = lines
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

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
            issue.dependencies = split_list_content(&content);
        }
        Section::Unknown => {
            // Ignore unknown sections
        }
    }
}

/// Split content on commas or whitespace for labels/deps.
fn split_list_content(content: &str) -> Vec<String> {
    // First try splitting on commas
    if content.contains(',') {
        content
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        // Otherwise split on whitespace
        content
            .split_whitespace()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// Validate a dependency type string.
///
/// Returns the dependency type if valid, or None if invalid.
pub fn validate_dependency_type(dep_type: &str) -> Option<&str> {
    match dep_type.to_lowercase().as_str() {
        "blocks" | "blocked-by" | "parent-child" | "related" | "duplicates" => Some(dep_type),
        _ => None,
    }
}

/// Parse a dependency string into (type, id).
///
/// Accepts "type:id" or bare "id" (defaults to "blocks").
///
/// Returns (dep_type, dep_id, is_valid_type) where is_valid_type indicates
/// whether the type was recognized.
pub fn parse_dependency(dep_str: &str) -> (String, String, bool) {
    if let Some((type_part, id_part)) = dep_str.split_once(':') {
        let is_valid = validate_dependency_type(type_part).is_some();
        (type_part.to_string(), id_part.to_string(), is_valid)
    } else {
        ("blocks".to_string(), dep_str.to_string(), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_issue() {
        let content = r#"## My First Issue
### Description
This is the description.

### Priority
1

### Type
bug
"#;
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "My First Issue");
        assert_eq!(
            issues[0].description,
            Some("This is the description.".to_string())
        );
        assert_eq!(issues[0].priority, Some("1".to_string()));
        assert_eq!(issues[0].issue_type, Some("bug".to_string()));
    }

    #[test]
    fn test_parse_multiple_issues() {
        let content = r#"## Issue One
### Type
task

## Issue Two
### Type
feature

## Issue Three
### Type
bug
"#;
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues.len(), 3);
        assert_eq!(issues[0].title, "Issue One");
        assert_eq!(issues[1].title, "Issue Two");
        assert_eq!(issues[2].title, "Issue Three");
    }

    #[test]
    fn test_implicit_description_quirk() {
        // Only first non-empty line before H3 is captured
        let content = r#"## Issue Title
First line becomes description
This line is ignored
And this one too

### Priority
2
"#;
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(
            issues[0].description,
            Some("First line becomes description".to_string())
        );
    }

    #[test]
    fn test_labels_comma_separated() {
        let content = r#"## Test Issue
### Labels
bug, urgent, frontend
"#;
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues[0].labels, vec!["bug", "urgent", "frontend"]);
    }

    #[test]
    fn test_labels_whitespace_separated() {
        let content = r#"## Test Issue
### Labels
bug urgent frontend
"#;
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues[0].labels, vec!["bug", "urgent", "frontend"]);
    }

    #[test]
    fn test_dependencies_parsing() {
        let content = r#"## Test Issue
### Dependencies
blocks:bd-123, bd-456, related:bd-789
"#;
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(
            issues[0].dependencies,
            vec!["blocks:bd-123", "bd-456", "related:bd-789"]
        );
    }

    #[test]
    fn test_acceptance_criteria_alias() {
        let content = r#"## Test Issue
### Acceptance
- [ ] First criterion
- [ ] Second criterion
"#;
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
        let content = r#"## Test Issue
### Unknown Section
This content should be ignored.

### Description
This is the actual description.
"#;
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
    fn test_parse_dependency() {
        let (t, id, valid) = parse_dependency("blocks:bd-123");
        assert_eq!(t, "blocks");
        assert_eq!(id, "bd-123");
        assert!(valid);

        let (t, id, valid) = parse_dependency("bd-456");
        assert_eq!(t, "blocks");
        assert_eq!(id, "bd-456");
        assert!(valid);

        let (t, id, valid) = parse_dependency("invalid:bd-789");
        assert_eq!(t, "invalid");
        assert_eq!(id, "bd-789");
        assert!(!valid);
    }

    #[test]
    fn test_design_section() {
        let content = r#"## Test Issue
### Design
Design notes here.
Multi-line content.
"#;
        let issues = parse_markdown_content(content).unwrap();
        assert!(issues[0].design.is_some());
        assert!(issues[0].design.as_ref().unwrap().contains("Design notes"));
    }

    #[test]
    fn test_case_insensitive_sections() {
        let content = r#"## Test Issue
### PRIORITY
1

### description
Test desc

### TYPE
task
"#;
        let issues = parse_markdown_content(content).unwrap();
        assert_eq!(issues[0].priority, Some("1".to_string()));
        assert_eq!(issues[0].description, Some("Test desc".to_string()));
        assert_eq!(issues[0].issue_type, Some("task".to_string()));
    }

    #[test]
    fn test_explicit_description_overrides_implicit() {
        let content = r#"## Test Issue
Implicit description line

### Description
Explicit description content
"#;
        let issues = parse_markdown_content(content).unwrap();
        // Explicit ### Description section should be used
        assert_eq!(
            issues[0].description,
            Some("Explicit description content".to_string())
        );
    }
}
