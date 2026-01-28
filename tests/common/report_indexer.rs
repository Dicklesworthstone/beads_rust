//! Artifact Report Indexer
//!
//! Generates human-friendly HTML/Markdown reports from test artifacts for faster triage.
//! Summarizes per-suite results, durations, failures, and artifact paths.
//!
//! Task: beads_rust-x7on

#![allow(clippy::cast_precision_loss)] // Expected for percentage calculations
#![allow(clippy::format_push_string)] // Clearer than write! for string building
#![allow(clippy::too_many_lines)] // Report generators are naturally long
#![allow(clippy::uninlined_format_args)] // Clear format strings

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

// ============================================================================
// DATA STRUCTURES
// ============================================================================

/// Metrics for a single command execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandMetric {
    pub label: String,
    pub binary: String,
    pub args: Vec<String>,
    pub exit_code: i32,
    pub success: bool,
    pub duration_ms: u128,
    pub stdout_len: usize,
    pub stderr_len: usize,
    pub stdout_path: Option<String>,
    pub stderr_path: Option<String>,
}

/// Metrics for a file tree snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMetric {
    pub label: String,
    pub file_count: usize,
    pub total_size: u64,
    pub snapshot_path: String,
}

/// Report for a single test
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReport {
    pub suite: String,
    pub test: String,
    pub passed: bool,
    pub run_count: usize,
    pub timestamp: Option<String>,
    pub total_duration_ms: u128,
    pub commands: Vec<CommandMetric>,
    pub snapshots: Vec<SnapshotMetric>,
    pub failure_reason: Option<String>,
    pub artifact_dir: PathBuf,
}

impl TestReport {
    pub fn failed_commands(&self) -> Vec<&CommandMetric> {
        self.commands.iter().filter(|c| !c.success).collect()
    }

    pub fn slowest_command(&self) -> Option<&CommandMetric> {
        self.commands.iter().max_by_key(|c| c.duration_ms)
    }
}

/// Aggregated report for a test suite
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteReport {
    pub name: String,
    pub tests: Vec<TestReport>,
    pub passed_count: usize,
    pub failed_count: usize,
    pub total_duration_ms: u128,
}

impl SuiteReport {
    pub fn from_tests(name: String, tests: Vec<TestReport>) -> Self {
        let passed_count = tests.iter().filter(|t| t.passed).count();
        let failed_count = tests.iter().filter(|t| !t.passed).count();
        let total_duration_ms = tests.iter().map(|t| t.total_duration_ms).sum();

        Self {
            name,
            tests,
            passed_count,
            failed_count,
            total_duration_ms,
        }
    }

    pub fn pass_rate(&self) -> f64 {
        if self.tests.is_empty() {
            return 100.0;
        }
        (self.passed_count as f64 / self.tests.len() as f64) * 100.0
    }

    pub fn failed_tests(&self) -> Vec<&TestReport> {
        self.tests.iter().filter(|t| !t.passed).collect()
    }
}

/// Complete report across all suites
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullReport {
    pub generated_at: String,
    pub artifact_root: PathBuf,
    pub suites: Vec<SuiteReport>,
    pub total_tests: usize,
    pub total_passed: usize,
    pub total_failed: usize,
    pub total_duration_ms: u128,
}

impl FullReport {
    pub fn pass_rate(&self) -> f64 {
        if self.total_tests == 0 {
            return 100.0;
        }
        (self.total_passed as f64 / self.total_tests as f64) * 100.0
    }

    pub fn failed_tests(&self) -> Vec<&TestReport> {
        self.suites.iter().flat_map(|s| s.failed_tests()).collect()
    }

    pub fn slowest_tests(&self, limit: usize) -> Vec<&TestReport> {
        let mut all_tests: Vec<_> = self.suites.iter().flat_map(|s| s.tests.iter()).collect();
        all_tests.sort_by_key(|b| std::cmp::Reverse(b.total_duration_ms));
        all_tests.into_iter().take(limit).collect()
    }

    pub fn tests_by_suite(&self, suite_name: &str) -> Option<&SuiteReport> {
        self.suites.iter().find(|s| s.name == suite_name)
    }
}

// ============================================================================
// JSONL PARSING (reuse types from harness/artifact_validator)
// ============================================================================

/// JSONL event entry (matches `harness::RunEvent`)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunEvent {
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
    #[serde(default)]
    pub stdout_path: Option<String>,
    #[serde(default)]
    pub stderr_path: Option<String>,
    #[serde(default)]
    pub snapshot_path: Option<String>,
}

/// File entry in snapshot files
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileEntry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Test summary from summary.json
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Summary {
    pub suite: String,
    pub test: String,
    pub passed: bool,
    pub run_count: usize,
    #[serde(default)]
    pub timestamp: Option<String>,
}

// ============================================================================
// ARTIFACT INDEXER
// ============================================================================

/// Indexer configuration
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// Base path for artifacts (default: target/test-artifacts)
    pub artifact_root: PathBuf,
    /// Include failed tests only in report
    pub failures_only: bool,
    /// Maximum number of tests to include in report (0 = unlimited)
    pub max_tests: usize,
    /// Include command details in report
    pub include_commands: bool,
    /// Include snapshot details in report
    pub include_snapshots: bool,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            artifact_root: PathBuf::from("target/test-artifacts"),
            failures_only: false,
            max_tests: 0,
            include_commands: true,
            include_snapshots: true,
        }
    }
}

/// Artifact report indexer
pub struct ArtifactIndexer {
    config: IndexerConfig,
}

impl ArtifactIndexer {
    pub fn new(artifact_root: impl Into<PathBuf>) -> Self {
        Self {
            config: IndexerConfig {
                artifact_root: artifact_root.into(),
                ..Default::default()
            },
        }
    }

    pub const fn with_config(config: IndexerConfig) -> Self {
        Self { config }
    }

    /// Generate a complete report by walking the artifact directory
    pub fn generate_report(&self) -> Result<FullReport, IndexerError> {
        if !self.config.artifact_root.exists() {
            return Err(IndexerError::ArtifactDirNotFound(
                self.config.artifact_root.clone(),
            ));
        }

        let mut suite_tests: HashMap<String, Vec<TestReport>> = HashMap::new();

        // Walk the artifact directory to find summary.json files
        for entry in WalkDir::new(&self.config.artifact_root)
            .min_depth(2) // suite/test level
            .max_depth(3)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();

            // Look for summary.json to identify test directories
            if path.file_name() == Some("summary.json".as_ref()) {
                let test_dir = path.parent().unwrap();
                if let Ok(report) = self.index_test_dir(test_dir) {
                    if self.config.failures_only && report.passed {
                        continue;
                    }

                    suite_tests
                        .entry(report.suite.clone())
                        .or_default()
                        .push(report);
                }
            }
        }

        // Build suite reports
        let mut suites: Vec<SuiteReport> = suite_tests
            .into_iter()
            .map(|(name, tests)| SuiteReport::from_tests(name, tests))
            .collect();

        // Sort suites by name
        suites.sort_by(|a, b| a.name.cmp(&b.name));

        // Apply max_tests limit if set
        if self.config.max_tests > 0 {
            for suite in &mut suites {
                if suite.tests.len() > self.config.max_tests {
                    suite.tests.truncate(self.config.max_tests);
                }
            }
        }

        // Calculate totals
        let total_tests: usize = suites.iter().map(|s| s.tests.len()).sum();
        let total_passed: usize = suites.iter().map(|s| s.passed_count).sum();
        let total_failed: usize = suites.iter().map(|s| s.failed_count).sum();
        let total_duration_ms: u128 = suites.iter().map(|s| s.total_duration_ms).sum();

        Ok(FullReport {
            generated_at: Utc::now().to_rfc3339(),
            artifact_root: self.config.artifact_root.clone(),
            suites,
            total_tests,
            total_passed,
            total_failed,
            total_duration_ms,
        })
    }

    /// Index a single test directory
    fn index_test_dir(&self, test_dir: &Path) -> Result<TestReport, IndexerError> {
        // Parse summary.json
        let summary_path = test_dir.join("summary.json");
        let summary: Summary = self.read_json(&summary_path)?;

        // Parse events.jsonl
        let events_path = test_dir.join("events.jsonl");
        let events = self.read_events(&events_path).unwrap_or_default();

        // Extract command metrics
        let mut commands: Vec<CommandMetric> = Vec::new();
        let mut snapshots: Vec<SnapshotMetric> = Vec::new();
        let mut total_duration_ms: u128 = 0;
        let mut failure_reason: Option<String> = None;

        for event in events {
            if event.event_type == "command" {
                total_duration_ms += event.duration_ms;

                if self.config.include_commands {
                    // If command failed, try to get failure reason from stderr
                    if !event.success && failure_reason.is_none() {
                        if let Some(ref stderr_path) = event.stderr_path {
                            let full_path = if Path::new(stderr_path).is_absolute() {
                                PathBuf::from(stderr_path)
                            } else {
                                test_dir.join(stderr_path)
                            };
                            if let Ok(stderr) = fs::read_to_string(&full_path) {
                                let preview: String =
                                    stderr.lines().take(5).collect::<Vec<_>>().join("\n");
                                failure_reason = Some(preview);
                            }
                        }
                    }

                    commands.push(CommandMetric {
                        label: event.label,
                        binary: event.binary,
                        args: event.args,
                        exit_code: event.exit_code,
                        success: event.success,
                        duration_ms: event.duration_ms,
                        stdout_len: event.stdout_len,
                        stderr_len: event.stderr_len,
                        stdout_path: event.stdout_path,
                        stderr_path: event.stderr_path,
                    });
                }
            } else if event.event_type == "snapshot" && self.config.include_snapshots {
                // Parse snapshot file to get file count and total size
                if let Some(ref snapshot_path) = event.snapshot_path {
                    let full_path = if Path::new(snapshot_path).is_absolute() {
                        PathBuf::from(snapshot_path)
                    } else {
                        test_dir.join(snapshot_path)
                    };

                    let (file_count, total_size) = self.parse_snapshot(&full_path);

                    snapshots.push(SnapshotMetric {
                        label: event.label,
                        file_count,
                        total_size,
                        snapshot_path: snapshot_path.clone(),
                    });
                }
            }
        }

        Ok(TestReport {
            suite: summary.suite,
            test: summary.test,
            passed: summary.passed,
            run_count: summary.run_count,
            timestamp: summary.timestamp,
            total_duration_ms,
            commands,
            snapshots,
            failure_reason,
            artifact_dir: test_dir.to_path_buf(),
        })
    }

    /// Read and parse a JSON file
    #[allow(clippy::unused_self)]
    fn read_json<T: for<'de> Deserialize<'de>>(&self, path: &Path) -> Result<T, IndexerError> {
        let content = fs::read_to_string(path).map_err(|e| IndexerError::IoError {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

        serde_json::from_str(&content).map_err(|e| IndexerError::ParseError {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// Read and parse events.jsonl
    #[allow(clippy::unused_self)]
    fn read_events(&self, path: &Path) -> Result<Vec<RunEvent>, IndexerError> {
        let content = fs::read_to_string(path).map_err(|e| IndexerError::IoError {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

        let mut events = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<RunEvent>(line) {
                events.push(event);
            }
        }

        Ok(events)
    }

    /// Parse a snapshot file and return (`file_count`, `total_size`)
    #[allow(clippy::unused_self)]
    fn parse_snapshot(&self, path: &Path) -> (usize, u64) {
        if let Ok(content) = fs::read_to_string(path) {
            if let Ok(entries) = serde_json::from_str::<Vec<FileEntry>>(&content) {
                let file_count = entries.iter().filter(|e| !e.is_dir).count();
                let total_size: u64 = entries.iter().map(|e| e.size).sum();
                return (file_count, total_size);
            }
        }
        (0, 0)
    }
}

// ============================================================================
// ERROR TYPES
// ============================================================================

/// Indexer errors
#[derive(Debug)]
pub enum IndexerError {
    ArtifactDirNotFound(PathBuf),
    IoError { path: PathBuf, message: String },
    ParseError { path: PathBuf, message: String },
}

impl std::fmt::Display for IndexerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ArtifactDirNotFound(path) => {
                write!(f, "Artifact directory not found: {}", path.display())
            }
            Self::IoError { path, message } => {
                write!(f, "IO error reading {}: {}", path.display(), message)
            }
            Self::ParseError { path, message } => {
                write!(f, "Parse error in {}: {}", path.display(), message)
            }
        }
    }
}

impl std::error::Error for IndexerError {}

// ============================================================================
// REPORT GENERATORS
// ============================================================================

/// Generate a Markdown report from the full report
pub fn generate_markdown_report(report: &FullReport) -> String {
    let mut md = String::new();

    // Header
    md.push_str("# Test Artifact Report\n\n");
    md.push_str(&format!(
        "**Generated:** {}\n\n",
        &report.generated_at[..19].replace('T', " ")
    ));

    // Summary
    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    md.push_str(&format!("| Total Tests | {} |\n", report.total_tests));
    md.push_str(&format!(
        "| Passed | {} ({:.1}%) |\n",
        report.total_passed,
        report.pass_rate()
    ));
    md.push_str(&format!("| Failed | {} |\n", report.total_failed));
    md.push_str(&format!(
        "| Duration | {:.2}s |\n\n",
        report.total_duration_ms as f64 / 1000.0
    ));

    // Suite breakdown
    md.push_str("## Suites\n\n");
    for suite in &report.suites {
        let status = if suite.failed_count == 0 {
            "✅"
        } else {
            "❌"
        };
        md.push_str(&format!(
            "### {} {} ({}/{} passed, {:.2}s)\n\n",
            status,
            suite.name,
            suite.passed_count,
            suite.tests.len(),
            suite.total_duration_ms as f64 / 1000.0
        ));

        // Test table for this suite
        md.push_str("| Test | Status | Duration | Commands |\n");
        md.push_str("|------|--------|----------|----------|\n");

        for test in &suite.tests {
            let status = if test.passed { "✅ Pass" } else { "❌ Fail" };
            md.push_str(&format!(
                "| {} | {} | {:.2}s | {} |\n",
                test.test,
                status,
                test.total_duration_ms as f64 / 1000.0,
                test.commands.len()
            ));
        }
        md.push('\n');
    }

    // Failed tests detail
    let failed = report.failed_tests();
    if !failed.is_empty() {
        md.push_str("## Failed Tests Detail\n\n");
        for test in failed {
            md.push_str(&format!("### `{}/{}`\n\n", test.suite, test.test));

            if let Some(ref reason) = test.failure_reason {
                md.push_str("**Failure reason:**\n```\n");
                md.push_str(reason);
                md.push_str("\n```\n\n");
            }

            // Show failed commands
            let failed_cmds = test.failed_commands();
            if !failed_cmds.is_empty() {
                md.push_str("**Failed commands:**\n\n");
                for cmd in failed_cmds {
                    md.push_str(&format!(
                        "- `{} {}` (exit {})\n",
                        cmd.binary,
                        cmd.args.join(" "),
                        cmd.exit_code
                    ));
                    if let Some(ref stderr) = cmd.stderr_path {
                        md.push_str(&format!("  - stderr: `{}`\n", stderr));
                    }
                }
                md.push('\n');
            }

            md.push_str(&format!(
                "**Artifacts:** `{}`\n\n",
                test.artifact_dir.display()
            ));
        }
    }

    // Slowest tests
    md.push_str("## Slowest Tests\n\n");
    md.push_str("| Test | Suite | Duration |\n");
    md.push_str("|------|-------|----------|\n");
    for test in report.slowest_tests(10) {
        md.push_str(&format!(
            "| {} | {} | {:.2}s |\n",
            test.test,
            test.suite,
            test.total_duration_ms as f64 / 1000.0
        ));
    }
    md.push('\n');

    md
}

/// Generate an HTML report from the full report
pub fn generate_html_report(report: &FullReport) -> String {
    let mut html = String::new();

    html.push_str(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <title>Test Artifact Report</title>
    <style>
        body {
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Oxygen, Ubuntu, sans-serif;
            max-width: 1200px;
            margin: 0 auto;
            padding: 20px;
            background: #f5f5f5;
        }
        h1 { color: #333; border-bottom: 2px solid #333; padding-bottom: 10px; }
        h2 { color: #555; margin-top: 30px; }
        h3 { color: #666; }
        .summary {
            display: grid;
            grid-template-columns: repeat(4, 1fr);
            gap: 15px;
            margin: 20px 0;
        }
        .stat-card {
            background: white;
            padding: 20px;
            border-radius: 8px;
            box-shadow: 0 2px 4px rgba(0,0,0,0.1);
            text-align: center;
        }
        .stat-value { font-size: 2em; font-weight: bold; }
        .stat-label { color: #666; margin-top: 5px; }
        .passed { color: #22c55e; }
        .failed { color: #ef4444; }
        table {
            width: 100%;
            border-collapse: collapse;
            background: white;
            margin: 15px 0;
            box-shadow: 0 2px 4px rgba(0,0,0,0.1);
        }
        th, td {
            padding: 12px;
            text-align: left;
            border-bottom: 1px solid #eee;
        }
        th {
            background: #f8f9fa;
            font-weight: 600;
        }
        tr:hover { background: #f8f9fa; }
        .status-pass { color: #22c55e; }
        .status-fail { color: #ef4444; font-weight: bold; }
        .failure-detail {
            background: #fff0f0;
            border-left: 4px solid #ef4444;
            padding: 15px;
            margin: 15px 0;
        }
        pre {
            background: #1e1e1e;
            color: #d4d4d4;
            padding: 15px;
            border-radius: 4px;
            overflow-x: auto;
        }
        .suite-header {
            display: flex;
            align-items: center;
            gap: 10px;
        }
        .badge {
            padding: 4px 8px;
            border-radius: 4px;
            font-size: 0.85em;
        }
        .badge-pass { background: #dcfce7; color: #166534; }
        .badge-fail { background: #fee2e2; color: #991b1b; }
        .artifact-link {
            font-family: monospace;
            font-size: 0.9em;
            color: #2563eb;
        }
    </style>
</head>
<body>
"#,
    );

    // Header
    html.push_str(&format!(
        "<h1>Test Artifact Report</h1>\n<p>Generated: {}</p>\n",
        &report.generated_at[..19].replace('T', " ")
    ));

    // Summary cards
    html.push_str(r#"<div class="summary">"#);
    html.push_str(&format!(
        r#"<div class="stat-card">
            <div class="stat-value">{}</div>
            <div class="stat-label">Total Tests</div>
        </div>"#,
        report.total_tests
    ));
    html.push_str(&format!(
        r#"<div class="stat-card">
            <div class="stat-value passed">{}</div>
            <div class="stat-label">Passed ({:.1}%)</div>
        </div>"#,
        report.total_passed,
        report.pass_rate()
    ));
    html.push_str(&format!(
        r#"<div class="stat-card">
            <div class="stat-value failed">{}</div>
            <div class="stat-label">Failed</div>
        </div>"#,
        report.total_failed
    ));
    html.push_str(&format!(
        r#"<div class="stat-card">
            <div class="stat-value">{:.2}s</div>
            <div class="stat-label">Duration</div>
        </div>"#,
        report.total_duration_ms as f64 / 1000.0
    ));
    html.push_str("</div>\n");

    // Suites
    html.push_str("<h2>Test Suites</h2>\n");
    for suite in &report.suites {
        let badge = if suite.failed_count == 0 {
            r#"<span class="badge badge-pass">PASS</span>"#
        } else {
            r#"<span class="badge badge-fail">FAIL</span>"#
        };

        html.push_str(&format!(
            r#"<h3 class="suite-header">{} {} <small>({}/{} passed, {:.2}s)</small></h3>"#,
            badge,
            suite.name,
            suite.passed_count,
            suite.tests.len(),
            suite.total_duration_ms as f64 / 1000.0
        ));

        html.push_str(
            "<table>\n<tr><th>Test</th><th>Status</th><th>Duration</th><th>Commands</th></tr>\n",
        );
        for test in &suite.tests {
            let status_class = if test.passed {
                "status-pass"
            } else {
                "status-fail"
            };
            let status_text = if test.passed { "Pass" } else { "Fail" };
            html.push_str(&format!(
                "<tr><td>{}</td><td class=\"{}\">{}</td><td>{:.2}s</td><td>{}</td></tr>\n",
                test.test,
                status_class,
                status_text,
                test.total_duration_ms as f64 / 1000.0,
                test.commands.len()
            ));
        }
        html.push_str("</table>\n");
    }

    // Failed tests detail
    let failed = report.failed_tests();
    if !failed.is_empty() {
        html.push_str("<h2>Failed Tests Detail</h2>\n");
        for test in failed {
            html.push_str(&format!(
                r#"<div class="failure-detail">
                <h3>{}/{}</h3>"#,
                test.suite, test.test
            ));

            if let Some(ref reason) = test.failure_reason {
                html.push_str("<p><strong>Failure reason:</strong></p>\n<pre>");
                html.push_str(&html_escape(reason));
                html.push_str("</pre>\n");
            }

            // Failed commands
            let failed_cmds = test.failed_commands();
            if !failed_cmds.is_empty() {
                html.push_str("<p><strong>Failed commands:</strong></p>\n<ul>\n");
                for cmd in failed_cmds {
                    html.push_str(&format!(
                        "<li><code>{} {}</code> (exit {})",
                        cmd.binary,
                        cmd.args.join(" "),
                        cmd.exit_code
                    ));
                    if let Some(ref stderr) = cmd.stderr_path {
                        html.push_str(&format!(
                            " - <span class=\"artifact-link\">{}</span>",
                            stderr
                        ));
                    }
                    html.push_str("</li>\n");
                }
                html.push_str("</ul>\n");
            }

            html.push_str(&format!(
                r#"<p><strong>Artifacts:</strong> <span class="artifact-link">{}</span></p>
                </div>"#,
                test.artifact_dir.display()
            ));
        }
    }

    // Slowest tests
    html.push_str("<h2>Slowest Tests</h2>\n");
    html.push_str("<table>\n<tr><th>Test</th><th>Suite</th><th>Duration</th></tr>\n");
    for test in report.slowest_tests(10) {
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{:.2}s</td></tr>\n",
            test.test,
            test.suite,
            test.total_duration_ms as f64 / 1000.0
        ));
    }
    html.push_str("</table>\n");

    html.push_str("</body>\n</html>\n");
    html
}

/// Escape HTML special characters
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Write report to files
pub fn write_reports(
    report: &FullReport,
    output_dir: &Path,
) -> Result<(PathBuf, PathBuf), std::io::Error> {
    fs::create_dir_all(output_dir)?;

    let md_path = output_dir.join("report.md");
    let html_path = output_dir.join("report.html");

    fs::write(&md_path, generate_markdown_report(report))?;
    fs::write(&html_path, generate_html_report(report))?;

    Ok((md_path, html_path))
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_artifacts(dir: &Path) -> std::io::Result<()> {
        // Create suite/test structure
        let test_dir = dir.join("test_suite").join("test_example");
        fs::create_dir_all(&test_dir)?;

        // Create summary.json
        let summary = r#"{"suite":"test_suite","test":"test_example","passed":true,"run_count":3}"#;
        fs::write(test_dir.join("summary.json"), summary)?;

        // Create events.jsonl
        let events = r#"{"timestamp":"2026-01-17T12:00:00Z","event_type":"command","label":"init","binary":"br","args":["init"],"cwd":"/tmp","exit_code":0,"success":true,"duration_ms":100,"stdout_len":50,"stderr_len":0}
{"timestamp":"2026-01-17T12:00:01Z","event_type":"command","label":"create","binary":"br","args":["create","test"],"cwd":"/tmp","exit_code":0,"success":true,"duration_ms":200,"stdout_len":100,"stderr_len":0}"#;
        fs::write(test_dir.join("events.jsonl"), events)?;

        Ok(())
    }

    #[test]
    fn test_indexer_parses_artifacts() {
        let temp_dir = TempDir::new().unwrap();
        create_test_artifacts(temp_dir.path()).unwrap();

        let indexer = ArtifactIndexer::new(temp_dir.path());
        let report = indexer.generate_report().unwrap();

        assert_eq!(report.total_tests, 1);
        assert_eq!(report.total_passed, 1);
        assert_eq!(report.total_failed, 0);
        assert_eq!(report.suites.len(), 1);
        assert_eq!(report.suites[0].name, "test_suite");
        assert_eq!(report.suites[0].tests[0].test, "test_example");
        assert_eq!(report.suites[0].tests[0].commands.len(), 2);
    }

    #[test]
    fn test_markdown_report_generation() {
        let temp_dir = TempDir::new().unwrap();
        create_test_artifacts(temp_dir.path()).unwrap();

        let indexer = ArtifactIndexer::new(temp_dir.path());
        let report = indexer.generate_report().unwrap();
        let md = generate_markdown_report(&report);

        assert!(md.contains("# Test Artifact Report"));
        assert!(md.contains("test_suite"));
        assert!(md.contains("test_example"));
        assert!(md.contains("✅"));
    }

    #[test]
    fn test_html_report_generation() {
        let temp_dir = TempDir::new().unwrap();
        create_test_artifacts(temp_dir.path()).unwrap();

        let indexer = ArtifactIndexer::new(temp_dir.path());
        let report = indexer.generate_report().unwrap();
        let html = generate_html_report(&report);

        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("Test Artifact Report"));
        assert!(html.contains("test_suite"));
        assert!(html.contains("test_example"));
    }

    #[test]
    fn test_failures_only_filter() {
        let temp_dir = TempDir::new().unwrap();
        create_test_artifacts(temp_dir.path()).unwrap();

        let config = IndexerConfig {
            artifact_root: temp_dir.path().to_path_buf(),
            failures_only: true,
            ..Default::default()
        };

        let indexer = ArtifactIndexer::with_config(config);
        let report = indexer.generate_report().unwrap();

        // Should have no tests since the only test passes
        assert_eq!(report.total_tests, 0);
    }

    #[test]
    fn test_artifact_dir_not_found() {
        let indexer = ArtifactIndexer::new("/nonexistent/path");
        let result = indexer.generate_report();

        assert!(matches!(result, Err(IndexerError::ArtifactDirNotFound(_))));
    }
}
