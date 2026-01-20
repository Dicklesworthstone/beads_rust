//! CSV formatting for `beads_rust`.
//!
//! Provides CSV output for list/export commands. Handles proper escaping
//! of fields containing commas, quotes, or newlines.

use crate::model::Issue;
use std::io::{self, Write};

/// Default fields for CSV export.
pub const DEFAULT_FIELDS: &[&str] = &[
    "id",
    "title",
    "status",
    "priority",
    "issue_type",
    "assignee",
    "created_at",
    "updated_at",
];

/// All available fields for CSV export.
pub const ALL_FIELDS: &[&str] = &[
    "id",
    "title",
    "description",
    "status",
    "priority",
    "issue_type",
    "assignee",
    "owner",
    "created_at",
    "updated_at",
    "closed_at",
    "due_at",
    "defer_until",
    "notes",
    "external_ref",
];

/// Escape a CSV field value.
///
/// Wraps in double quotes if the value contains commas, quotes, or newlines.
/// Doubles any existing quotes within the value.
#[must_use]
pub fn escape_field(value: &str) -> String {
    let needs_quoting =
        value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r');

    if needs_quoting {
        let escaped = value.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}

/// Get a field value from an issue by field name.
#[must_use]
pub fn get_field_value(issue: &Issue, field: &str) -> String {
    match field {
        "id" => issue.id.clone(),
        "title" => issue.title.clone(),
        "description" => issue.description.clone().unwrap_or_default(),
        "status" => issue.status.as_str().to_string(),
        "priority" => issue.priority.0.to_string(),
        "issue_type" => issue.issue_type.as_str().to_string(),
        "assignee" => issue.assignee.clone().unwrap_or_default(),
        "owner" => issue.owner.clone().unwrap_or_default(),
        "created_at" => issue.created_at.to_rfc3339(),
        "updated_at" => issue.updated_at.to_rfc3339(),
        "closed_at" => issue
            .closed_at
            .map_or_else(String::new, |dt| dt.to_rfc3339()),
        "due_at" => issue.due_at.map_or_else(String::new, |dt| dt.to_rfc3339()),
        "defer_until" => issue
            .defer_until
            .map_or_else(String::new, |dt| dt.to_rfc3339()),
        "notes" => issue.notes.clone().unwrap_or_default(),
        "external_ref" => issue.external_ref.clone().unwrap_or_default(),
        _ => String::new(),
    }
}

/// Parse a comma-separated list of field names.
///
/// Returns the default fields if the input is empty.
#[must_use]
pub fn parse_fields(fields_arg: Option<&str>) -> Vec<&'static str> {
    match fields_arg {
        Some(arg) if !arg.is_empty() => arg
            .split(',')
            .map(str::trim)
            .filter_map(|f| ALL_FIELDS.iter().find(|&&af| af == f).copied())
            .collect(),
        _ => DEFAULT_FIELDS.to_vec(),
    }
}

/// Write CSV header row to the given writer.
///
/// # Errors
///
/// Returns an error if writing fails.
pub fn write_header<W: Write>(writer: &mut W, fields: &[&str]) -> io::Result<()> {
    let header = fields.join(",");
    writeln!(writer, "{header}")
}

/// Format a single issue as a CSV row.
#[must_use]
pub fn format_issue_row(issue: &Issue, fields: &[&str]) -> String {
    fields
        .iter()
        .map(|&field| escape_field(&get_field_value(issue, field)))
        .collect::<Vec<_>>()
        .join(",")
}

/// Write issues as CSV to the given writer.
///
/// # Errors
///
/// Returns an error if writing fails.
pub fn write_csv<W: Write>(writer: &mut W, issues: &[Issue], fields: &[&str]) -> io::Result<()> {
    write_header(writer, fields)?;
    for issue in issues {
        let row = format_issue_row(issue, fields);
        writeln!(writer, "{row}")?;
    }
    Ok(())
}

/// Format issues as a complete CSV string.
///
/// # Panics
///
/// Panics if writing to the in-memory buffer fails (which should not happen).
#[must_use]
pub fn format_csv(issues: &[Issue], fields: &[&str]) -> String {
    let mut output = Vec::new();
    // write_csv should not fail with Vec<u8>
    write_csv(&mut output, issues, fields).expect("writing to Vec should not fail");
    String::from_utf8_lossy(&output).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IssueType, Priority, Status};
    use chrono::{TimeZone, Utc};

    fn make_test_issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap(),
            created_by: None,
            updated_at: Utc.with_ymd_and_hms(2025, 1, 15, 14, 30, 0).unwrap(),
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        }
    }

    #[test]
    fn test_escape_field_plain() {
        assert_eq!(escape_field("simple"), "simple");
        assert_eq!(escape_field("hello world"), "hello world");
    }

    #[test]
    fn test_escape_field_with_comma() {
        assert_eq!(escape_field("hello, world"), "\"hello, world\"");
    }

    #[test]
    fn test_escape_field_with_quotes() {
        assert_eq!(escape_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn test_escape_field_with_newline() {
        assert_eq!(escape_field("line1\nline2"), "\"line1\nline2\"");
    }

    #[test]
    fn test_escape_field_mixed() {
        assert_eq!(
            escape_field("a, b \"and\" c\nd"),
            "\"a, b \"\"and\"\" c\nd\""
        );
    }

    #[test]
    fn test_get_field_value() {
        let mut issue = make_test_issue("bd-123", "Test Issue");
        issue.assignee = Some("alice".to_string());
        issue.status = Status::InProgress;

        assert_eq!(get_field_value(&issue, "id"), "bd-123");
        assert_eq!(get_field_value(&issue, "title"), "Test Issue");
        assert_eq!(get_field_value(&issue, "status"), "in_progress");
        assert_eq!(get_field_value(&issue, "priority"), "2");
        assert_eq!(get_field_value(&issue, "assignee"), "alice");
        assert_eq!(get_field_value(&issue, "unknown"), "");
    }

    #[test]
    fn test_parse_fields_default() {
        let fields = parse_fields(None);
        assert_eq!(fields, DEFAULT_FIELDS);
    }

    #[test]
    fn test_parse_fields_custom() {
        let fields = parse_fields(Some("id,title,status"));
        assert_eq!(fields, vec!["id", "title", "status"]);
    }

    #[test]
    fn test_parse_fields_filters_invalid() {
        let fields = parse_fields(Some("id,invalid,title"));
        assert_eq!(fields, vec!["id", "title"]);
    }

    #[test]
    fn test_format_issue_row() {
        let issue = make_test_issue("bd-456", "Simple Task");
        let fields = &["id", "title", "status"];
        let row = format_issue_row(&issue, fields);
        assert_eq!(row, "bd-456,Simple Task,open");
    }

    #[test]
    fn test_format_issue_row_with_comma_in_title() {
        let issue = make_test_issue("bd-789", "Fix bug, then test");
        let fields = &["id", "title"];
        let row = format_issue_row(&issue, fields);
        assert_eq!(row, "bd-789,\"Fix bug, then test\"");
    }

    #[test]
    fn test_format_csv() {
        let issues = vec![
            make_test_issue("bd-1", "First"),
            make_test_issue("bd-2", "Second"),
        ];
        let fields = &["id", "title"];
        let csv = format_csv(&issues, fields);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "id,title");
        assert_eq!(lines[1], "bd-1,First");
        assert_eq!(lines[2], "bd-2,Second");
    }

    #[test]
    fn test_write_header() {
        let mut output = Vec::new();
        write_header(&mut output, &["id", "title", "status"]).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "id,title,status\n");
    }
}
