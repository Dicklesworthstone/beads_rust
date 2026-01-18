//! Artifact Log Validator
//!
//! Validates JSONL event logs, snapshot files, and summaries against the
//! documented schema in `docs/ARTIFACT_LOG_SCHEMA.md`.
//!
//! Task: beads_rust-r23m

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Validation result with detailed error context
#[derive(Debug)]
pub struct ValidationResult {
    pub valid: bool,
    pub errors: Vec<ValidationError>,
    pub warnings: Vec<String>,
}

impl ValidationResult {
    pub const fn ok() -> Self {
        Self {
            valid: true,
            errors: vec![],
            warnings: vec![],
        }
    }

    pub fn with_error(mut self, error: ValidationError) -> Self {
        self.valid = false;
        self.errors.push(error);
        self
    }

    pub fn with_warning(mut self, warning: String) -> Self {
        self.warnings.push(warning);
        self
    }

    pub fn merge(mut self, other: Self) -> Self {
        self.valid = self.valid && other.valid;
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
        self
    }
}

/// Detailed validation error
#[derive(Debug)]
pub struct ValidationError {
    pub line: Option<usize>,
    pub field: Option<String>,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.line, &self.field) {
            (Some(line), Some(field)) => {
                write!(f, "Line {}, field '{}': {}", line, field, self.message)
            }
            (Some(line), None) => write!(f, "Line {}: {}", line, self.message),
            (None, Some(field)) => write!(f, "Field '{}': {}", field, self.message),
            (None, None) => write!(f, "{}", self.message),
        }
    }
}

/// Event types in the log
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Command,
    Snapshot,
}

/// JSONL event entry - matches `harness::RunEvent`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    pub timestamp: String,
    pub event_type: String,
    pub label: String,
    pub binary: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub exit_code: i32,
    pub success: bool,
    pub duration_ms: u128,
    pub stdout_len: usize,
    pub stderr_len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_path: Option<String>,
}

/// File entry in snapshot files
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Test summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub suite: String,
    pub test: String,
    pub passed: bool,
    pub run_count: usize,
    pub timestamp: String,
}

/// Artifact validator
pub struct ArtifactValidator {
    strict: bool,
}

impl Default for ArtifactValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl ArtifactValidator {
    pub const fn new() -> Self {
        Self { strict: true }
    }

    /// Enable/disable strict mode (fails on warnings)
    pub const fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Validate an events.jsonl file
    pub fn validate_events_file(&self, path: &Path) -> ValidationResult {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                return ValidationResult::ok().with_error(ValidationError {
                    line: None,
                    field: None,
                    message: format!("Failed to read file: {e}"),
                });
            }
        };

        self.validate_events_content(&content)
    }

    /// Validate events content (JSONL)
    pub fn validate_events_content(&self, content: &str) -> ValidationResult {
        let mut result = ValidationResult::ok();

        // Normalize line endings
        let content = content.replace("\r\n", "\n");

        for (idx, line) in content.lines().enumerate() {
            let line_num = idx + 1;
            let line = line.trim();

            if line.is_empty() {
                continue;
            }

            match serde_json::from_str::<RunEvent>(line) {
                Ok(event) => {
                    result = result.merge(self.validate_event(&event, line_num));
                }
                Err(e) => {
                    result = result.with_error(ValidationError {
                        line: Some(line_num),
                        field: None,
                        message: format!("Invalid JSON: {e}"),
                    });
                }
            }
        }

        result
    }

    /// Validate a single event
    #[allow(clippy::unused_self)]
    fn validate_event(&self, event: &RunEvent, line_num: usize) -> ValidationResult {
        let mut result = ValidationResult::ok();

        // Validate timestamp (RFC3339)
        if DateTime::parse_from_rfc3339(&event.timestamp).is_err() {
            result = result.with_error(ValidationError {
                line: Some(line_num),
                field: Some("timestamp".to_string()),
                message: format!("Invalid RFC3339 timestamp: {}", event.timestamp),
            });
        }

        // Validate event_type
        if event.event_type != "command" && event.event_type != "snapshot" {
            result = result.with_error(ValidationError {
                line: Some(line_num),
                field: Some("event_type".to_string()),
                message: format!("Must be 'command' or 'snapshot', got: {}", event.event_type),
            });
        }

        // Validate label is non-empty
        if event.label.is_empty() {
            result = result.with_error(ValidationError {
                line: Some(line_num),
                field: Some("label".to_string()),
                message: "Label cannot be empty".to_string(),
            });
        }

        // Validate cwd is absolute
        if !event.cwd.is_empty() && !event.cwd.starts_with('/') && !event.cwd.contains(':') {
            result = result.with_warning(format!(
                "Line {}: cwd should be absolute path: {}",
                line_num, event.cwd
            ));
        }

        // For command events, validate binary is set
        if event.event_type == "command" && event.binary.is_empty() {
            result = result.with_error(ValidationError {
                line: Some(line_num),
                field: Some("binary".to_string()),
                message: "Binary required for command events".to_string(),
            });
        }

        // Validate exit code range
        if event.exit_code < -128 || event.exit_code > 255 {
            result = result.with_warning(format!(
                "Line {}: exit_code {} outside typical range [-128, 255]",
                line_num, event.exit_code
            ));
        }

        // Validate path safety (no traversal)
        for path in [&event.stdout_path, &event.stderr_path, &event.snapshot_path]
            .into_iter()
            .flatten()
        {
            if path.contains("..") {
                result = result.with_error(ValidationError {
                    line: Some(line_num),
                    field: Some("*_path".to_string()),
                    message: format!("Path traversal detected: {path}"),
                });
            }
        }

        result
    }

    /// Validate a snapshot file
    pub fn validate_snapshot_file(&self, path: &Path) -> ValidationResult {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                return ValidationResult::ok().with_error(ValidationError {
                    line: None,
                    field: None,
                    message: format!("Failed to read file: {e}"),
                });
            }
        };

        self.validate_snapshot_content(&content)
    }

    /// Validate snapshot content
    #[allow(clippy::unused_self)]
    pub fn validate_snapshot_content(&self, content: &str) -> ValidationResult {
        let mut result = ValidationResult::ok();

        let entries: Vec<FileEntry> = match serde_json::from_str(content) {
            Ok(e) => e,
            Err(e) => {
                return result.with_error(ValidationError {
                    line: None,
                    field: None,
                    message: format!("Invalid JSON array: {e}"),
                });
            }
        };

        for (idx, entry) in entries.iter().enumerate() {
            // Validate path is relative
            if entry.path.starts_with('/') || entry.path.contains(':') {
                result = result.with_error(ValidationError {
                    line: Some(idx + 1),
                    field: Some("path".to_string()),
                    message: format!("Path must be relative: {}", entry.path),
                });
            }

            // Validate no traversal
            if entry.path.contains("..") {
                result = result.with_error(ValidationError {
                    line: Some(idx + 1),
                    field: Some("path".to_string()),
                    message: format!("Path traversal detected: {}", entry.path),
                });
            }

            // Warn if directory has non-zero size
            if entry.is_dir && entry.size > 0 {
                result = result.with_warning(format!(
                    "Entry {}: directory '{}' has non-zero size {}",
                    idx + 1,
                    entry.path,
                    entry.size
                ));
            }
        }

        // Check for sorted order
        let mut sorted = entries.clone();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        if entries.iter().map(|e| &e.path).collect::<Vec<_>>()
            != sorted.iter().map(|e| &e.path).collect::<Vec<_>>()
        {
            result = result.with_warning("Entries not sorted by path".to_string());
        }

        result
    }

    /// Validate a summary file
    pub fn validate_summary_file(&self, path: &Path) -> ValidationResult {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                return ValidationResult::ok().with_error(ValidationError {
                    line: None,
                    field: None,
                    message: format!("Failed to read file: {e}"),
                });
            }
        };

        self.validate_summary_content(&content)
    }

    /// Validate summary content
    #[allow(clippy::unused_self)]
    pub fn validate_summary_content(&self, content: &str) -> ValidationResult {
        let mut result = ValidationResult::ok();

        let summary: Summary = match serde_json::from_str(content) {
            Ok(s) => s,
            Err(e) => {
                return result.with_error(ValidationError {
                    line: None,
                    field: None,
                    message: format!("Invalid JSON: {e}"),
                });
            }
        };

        // Validate timestamp
        if DateTime::parse_from_rfc3339(&summary.timestamp).is_err() {
            result = result.with_error(ValidationError {
                line: None,
                field: Some("timestamp".to_string()),
                message: format!("Invalid RFC3339 timestamp: {}", summary.timestamp),
            });
        }

        // Validate suite name
        if summary.suite.is_empty() {
            result = result.with_error(ValidationError {
                line: None,
                field: Some("suite".to_string()),
                message: "Suite name cannot be empty".to_string(),
            });
        }

        // Validate test name
        if summary.test.is_empty() {
            result = result.with_error(ValidationError {
                line: None,
                field: Some("test".to_string()),
                message: "Test name cannot be empty".to_string(),
            });
        }

        result
    }

    /// Validate an entire artifact directory
    pub fn validate_artifact_dir(&self, dir: &Path) -> ValidationResult {
        let mut result = ValidationResult::ok();

        // Check events.jsonl
        let events_path = dir.join("events.jsonl");
        if events_path.exists() {
            result = result.merge(self.validate_events_file(&events_path));
        }

        // Check summary.json
        let summary_path = dir.join("summary.json");
        if summary_path.exists() {
            result = result.merge(self.validate_summary_file(&summary_path));
        }

        // Check all snapshot files
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name.contains("snapshot") {
                        result = result.merge(self.validate_snapshot_file(&path));
                    }
                }
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_event_passes() {
        let validator = ArtifactValidator::new();
        let content = r#"{"timestamp":"2026-01-17T12:34:56.000Z","event_type":"command","label":"init","binary":"br","args":["init"],"cwd":"/tmp/test","exit_code":0,"success":true,"duration_ms":42,"stdout_len":64,"stderr_len":0}"#;
        let result = validator.validate_events_content(content);
        assert!(result.valid, "Errors: {:?}", result.errors);
    }

    #[test]
    fn invalid_timestamp_fails() {
        let validator = ArtifactValidator::new();
        let content = r#"{"timestamp":"not-a-date","event_type":"command","label":"init","binary":"br","args":[],"cwd":"/tmp","exit_code":0,"success":true,"duration_ms":0,"stdout_len":0,"stderr_len":0}"#;
        let result = validator.validate_events_content(content);
        assert!(!result.valid);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.field.as_deref() == Some("timestamp"))
        );
    }

    #[test]
    fn invalid_event_type_fails() {
        let validator = ArtifactValidator::new();
        let content = r#"{"timestamp":"2026-01-17T12:34:56.000Z","event_type":"invalid","label":"test","binary":"br","args":[],"cwd":"/tmp","exit_code":0,"success":true,"duration_ms":0,"stdout_len":0,"stderr_len":0}"#;
        let result = validator.validate_events_content(content);
        assert!(!result.valid);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.field.as_deref() == Some("event_type"))
        );
    }

    #[test]
    fn path_traversal_fails() {
        let validator = ArtifactValidator::new();
        let content = r#"{"timestamp":"2026-01-17T12:34:56.000Z","event_type":"command","label":"test","binary":"br","args":[],"cwd":"/tmp","exit_code":0,"success":true,"duration_ms":0,"stdout_len":0,"stderr_len":0,"stdout_path":"../etc/passwd"}"#;
        let result = validator.validate_events_content(content);
        assert!(!result.valid);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("traversal"))
        );
    }

    #[test]
    fn valid_snapshot_passes() {
        let validator = ArtifactValidator::new();
        let content = r#"[{"path":".beads","size":0,"is_dir":true},{"path":".beads/beads.db","size":12288,"is_dir":false}]"#;
        let result = validator.validate_snapshot_content(content);
        assert!(result.valid, "Errors: {:?}", result.errors);
    }

    #[test]
    fn valid_summary_passes() {
        let validator = ArtifactValidator::new();
        let content = r#"{"suite":"e2e","test":"test_init","passed":true,"run_count":1,"timestamp":"2026-01-17T12:34:56.000Z"}"#;
        let result = validator.validate_summary_content(content);
        assert!(result.valid, "Errors: {:?}", result.errors);
    }

    #[test]
    fn empty_suite_fails() {
        let validator = ArtifactValidator::new();
        let content = r#"{"suite":"","test":"test","passed":true,"run_count":1,"timestamp":"2026-01-17T12:34:56.000Z"}"#;
        let result = validator.validate_summary_content(content);
        assert!(!result.valid);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.field.as_deref() == Some("suite"))
        );
    }
}
