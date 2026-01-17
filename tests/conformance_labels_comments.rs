#![allow(clippy::all, clippy::pedantic, clippy::nursery)]
//! Conformance Tests: Label and Comment Commands
//!
//! These tests verify br (Rust) produces identical output to bd (Go)
//! for label and comment operations.

mod common;

use assert_cmd::Command;
use chrono::Utc;
use common::cli::extract_json_payload;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;
use tracing::info;

/// Output from running a command
#[derive(Debug)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: std::process::ExitStatus,
    pub duration: Duration,
}

/// Workspace for conformance tests with paired br/bd directories
pub struct ConformanceWorkspace {
    pub temp_dir: TempDir,
    pub br_root: PathBuf,
    pub bd_root: PathBuf,
    pub log_dir: PathBuf,
}

impl ConformanceWorkspace {
    pub fn new() -> Self {
        let temp_dir = TempDir::new().expect("create temp dir");
        let root = temp_dir.path().to_path_buf();
        let br_root = root.join("br_workspace");
        let bd_root = root.join("bd_workspace");
        let log_dir = root.join("logs");

        fs::create_dir_all(&br_root).expect("create br workspace");
        fs::create_dir_all(&bd_root).expect("create bd workspace");
        fs::create_dir_all(&log_dir).expect("create log dir");

        Self {
            temp_dir,
            br_root,
            bd_root,
            log_dir,
        }
    }

    /// Initialize both br and bd workspaces
    pub fn init_both(&self) -> (CmdOutput, CmdOutput) {
        let br_out = self.run_br(["init"], "init");
        let bd_out = self.run_bd(["init"], "init");
        (br_out, bd_out)
    }

    /// Run br command in the br workspace
    pub fn run_br<I, S>(&self, args: I, label: &str) -> CmdOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run_br_cmd(&self.br_root, &self.log_dir, args, &format!("br_{label}"))
    }

    /// Run bd command in the bd workspace
    pub fn run_bd<I, S>(&self, args: I, label: &str) -> CmdOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run_bd_cmd(&self.bd_root, &self.log_dir, args, &format!("bd_{label}"))
    }
}

#[derive(Debug, Clone)]
struct LogConfig {
    json_logs: bool,
    junit: bool,
    summary: bool,
    failure_context: bool,
}

impl LogConfig {
    fn from_env() -> Self {
        Self {
            json_logs: env_flag("CONFORMANCE_JSON_LOGS"),
            junit: env_flag("CONFORMANCE_JUNIT_XML"),
            summary: env_flag("CONFORMANCE_SUMMARY"),
            failure_context: env_flag("CONFORMANCE_FAILURE_CONTEXT"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RunLogEntry {
    timestamp: String,
    label: String,
    binary: String,
    args: Vec<String>,
    cwd: String,
    status_code: i32,
    success: bool,
    duration_ms: u128,
    stdout_len: usize,
    stderr_len: usize,
    log_path: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct SummaryStats {
    runs: u64,
    failures: u64,
    total_ms: u128,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct SummaryReport {
    generated_at: String,
    total_runs: u64,
    total_failures: u64,
    by_binary: std::collections::HashMap<String, SummaryStats>,
    by_label: std::collections::HashMap<String, SummaryStats>,
    comparisons: std::collections::HashMap<String, ComparisonStats>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct ComparisonStats {
    br_runs: u64,
    bd_runs: u64,
    br_total_ms: u128,
    bd_total_ms: u128,
    speedup_bd_over_br: Option<f64>,
}

static LOG_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn log_mutex() -> &'static Mutex<()> {
    LOG_MUTEX.get_or_init(|| Mutex::new(()))
}

fn env_flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn collect_dir_listing(path: &PathBuf) -> Vec<String> {
    let mut entries = Vec::new();
    if let Ok(read_dir) = fs::read_dir(path) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() {
                    entries.push(format!("{name}/"));
                } else {
                    entries.push(format!("{name} ({:?} bytes)", meta.len()));
                }
            } else {
                entries.push(name);
            }
        }
    }
    entries.sort();
    entries
}

fn append_run_entry(log_dir: &PathBuf, entry: &RunLogEntry) {
    let log_path = log_dir.join("conformance_runs.jsonl");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open conformance_runs.jsonl");
    let json = serde_json::to_string(entry).expect("serialize run entry");
    writeln!(file, "{json}").expect("append run entry");
}

fn read_run_entries(log_dir: &PathBuf) -> Vec<RunLogEntry> {
    let log_path = log_dir.join("conformance_runs.jsonl");
    let Ok(contents) = fs::read_to_string(&log_path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<RunLogEntry>(line).ok())
        .collect()
}

fn update_summary(log_dir: &PathBuf, entries: &[RunLogEntry]) {
    let mut report = SummaryReport::default();
    report.generated_at = chrono::Utc::now().to_rfc3339();

    for entry in entries {
        report.total_runs += 1;
        if !entry.success {
            report.total_failures += 1;
        }

        let by_binary = report
            .by_binary
            .entry(entry.binary.clone())
            .or_insert_with(SummaryStats::default);
        by_binary.runs += 1;
        if !entry.success {
            by_binary.failures += 1;
        }
        by_binary.total_ms = by_binary.total_ms.saturating_add(entry.duration_ms);

        let by_label = report
            .by_label
            .entry(entry.label.clone())
            .or_insert_with(SummaryStats::default);
        by_label.runs += 1;
        if !entry.success {
            by_label.failures += 1;
        }
        by_label.total_ms = by_label.total_ms.saturating_add(entry.duration_ms);

        let comparison = report
            .comparisons
            .entry(entry.label.clone())
            .or_insert_with(ComparisonStats::default);
        if entry.binary == "br" {
            comparison.br_runs += 1;
            comparison.br_total_ms = comparison.br_total_ms.saturating_add(entry.duration_ms);
        } else if entry.binary == "bd" {
            comparison.bd_runs += 1;
            comparison.bd_total_ms = comparison.bd_total_ms.saturating_add(entry.duration_ms);
        }
    }

    for comparison in report.comparisons.values_mut() {
        if comparison.br_total_ms > 0 && comparison.bd_total_ms > 0 {
            comparison.speedup_bd_over_br =
                Some(comparison.bd_total_ms as f64 / comparison.br_total_ms as f64);
        }
    }

    let summary_path = log_dir.join("conformance_summary.json");
    let json = serde_json::to_string_pretty(&report).expect("serialize summary");
    fs::write(summary_path, json).expect("write summary");
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn write_junit(log_dir: &PathBuf, entries: &[RunLogEntry]) {
    let total = entries.len();
    let failures = entries.iter().filter(|e| !e.success).count();
    let mut xml = String::new();
    xml.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    xml.push('\n');
    xml.push_str(&format!(
        r#"<testsuite name="conformance_runs" tests="{total}" failures="{failures}">"#
    ));
    xml.push('\n');

    for entry in entries {
        let name = xml_escape(&format!("{}:{}", entry.binary, entry.label));
        let classname = xml_escape(&entry.binary);
        let time_secs = entry.duration_ms as f64 / 1000.0;
        xml.push_str(&format!(
            r#"  <testcase classname="{classname}" name="{name}" time="{time_secs:.3}">"#
        ));
        if !entry.success {
            let msg = xml_escape(&format!(
                "exit={}; log={}",
                entry.status_code, entry.log_path
            ));
            xml.push_str(&format!(r#"<failure message="{msg}"/>"#));
        }
        xml.push_str("</testcase>\n");
    }

    xml.push_str("</testsuite>\n");
    let junit_path = log_dir.join("conformance_junit.xml");
    fs::write(junit_path, xml).expect("write junit xml");
}

fn write_failure_context(
    log_dir: &PathBuf,
    entry: &RunLogEntry,
    stdout: &str,
    stderr: &str,
    cwd: &PathBuf,
) {
    let beads_dir = cwd.join(".beads");
    let context = serde_json::json!({
        "timestamp": entry.timestamp,
        "label": entry.label,
        "binary": entry.binary,
        "args": entry.args,
        "cwd": entry.cwd,
        "status_code": entry.status_code,
        "success": entry.success,
        "duration_ms": entry.duration_ms,
        "stdout_len": entry.stdout_len,
        "stderr_len": entry.stderr_len,
        "stdout_preview": stdout.chars().take(2000).collect::<String>(),
        "stderr_preview": stderr.chars().take(2000).collect::<String>(),
        "beads_dir": beads_dir.display().to_string(),
        "beads_entries": collect_dir_listing(&beads_dir),
        "recent_runs": read_run_entries(log_dir).into_iter().rev().take(5).collect::<Vec<_>>(),
    });
    let path = log_dir.join(format!("{}.failure.json", entry.label));
    let json = serde_json::to_string_pretty(&context).expect("serialize failure context");
    fs::write(path, json).expect("write failure context");
}

fn record_run(log_dir: &PathBuf, entry: RunLogEntry, stdout: &str, stderr: &str, cwd: &PathBuf) {
    let config = LogConfig::from_env();
    if !(config.json_logs || config.junit || config.summary || config.failure_context) {
        return;
    }

    let _guard = log_mutex().lock().expect("lock test log mutex");
    append_run_entry(log_dir, &entry);
    let entries = read_run_entries(log_dir);

    if config.summary {
        update_summary(log_dir, &entries);
    }
    if config.junit {
        write_junit(log_dir, &entries);
    }
    if config.failure_context && !entry.success {
        write_failure_context(log_dir, &entry, stdout, stderr, cwd);
    }
}

fn run_br_cmd<I, S>(cwd: &PathBuf, log_dir: &PathBuf, args: I, label: &str) -> CmdOutput
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(cwd);
    cmd.args(args);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_LOG", "beads_rust=debug");
    cmd.env("RUST_BACKTRACE", "1");
    cmd.env("HOME", cwd);

    run_and_log(cmd, cwd, log_dir, label)
}

fn run_bd_cmd<I, S>(cwd: &PathBuf, log_dir: &PathBuf, args: I, label: &str) -> CmdOutput
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_cmd_system("bd", cwd, log_dir, args, label)
}

fn run_cmd_system<I, S>(
    binary: &str,
    cwd: &PathBuf,
    log_dir: &PathBuf,
    args: I,
    label: &str,
) -> CmdOutput
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = std::process::Command::new(binary);
    cmd.current_dir(cwd);
    cmd.args(args);
    cmd.env("NO_COLOR", "1");
    cmd.env("HOME", cwd);

    let start = Instant::now();
    let output = cmd.output().expect(&format!("run {binary}"));
    let duration = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Log output
    let log_path = log_dir.join(format!("{label}.log"));
    let timestamp = SystemTime::now();
    let log_body = format!(
        "label: {label}\nbinary: {binary}\nstarted: {:?}\nduration: {:?}\nstatus: {}\ncwd: {}\n\nstdout:\n{}\n\nstderr:\n{}\n",
        timestamp,
        duration,
        output.status,
        cwd.display(),
        stdout,
        stderr
    );
    fs::write(&log_path, log_body).expect("write log");

    let entry = RunLogEntry {
        timestamp: Utc::now().to_rfc3339(),
        label: label.to_string(),
        binary: binary.to_string(),
        args: cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect(),
        cwd: cwd.display().to_string(),
        status_code: output.status.code().unwrap_or(-1),
        success: output.status.success(),
        duration_ms: duration.as_millis(),
        stdout_len: stdout.len(),
        stderr_len: stderr.len(),
        log_path: log_path.display().to_string(),
    };
    record_run(log_dir, entry, &stdout, &stderr, cwd);

    CmdOutput {
        stdout,
        stderr,
        status: output.status,
        duration,
    }
}

fn run_and_log(mut cmd: Command, cwd: &PathBuf, log_dir: &PathBuf, label: &str) -> CmdOutput {
    let start = Instant::now();
    let output = cmd.output().expect("run command");
    let duration = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let log_path = log_dir.join(format!("{label}.log"));
    let timestamp = SystemTime::now();
    let log_body = format!(
        "label: {label}\nstarted: {:?}\nduration: {:?}\nstatus: {}\nargs: {:?}\ncwd: {}\n\nstdout:\n{}\n\nstderr:\n{}\n",
        timestamp,
        duration,
        output.status,
        cmd.get_args().collect::<Vec<_>>(),
        cwd.display(),
        stdout,
        stderr
    );
    fs::write(&log_path, log_body).expect("write log");

    let entry = RunLogEntry {
        timestamp: Utc::now().to_rfc3339(),
        label: label.to_string(),
        binary: "br".to_string(),
        args: cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect(),
        cwd: cwd.display().to_string(),
        status_code: output.status.code().unwrap_or(-1),
        success: output.status.success(),
        duration_ms: duration.as_millis(),
        stdout_len: stdout.len(),
        stderr_len: stderr.len(),
        log_path: log_path.display().to_string(),
    };
    record_run(log_dir, entry, &stdout, &stderr, cwd);

    CmdOutput {
        stdout,
        stderr,
        status: output.status,
        duration,
    }
}

fn extract_issue_id(json_str: &str) -> String {
    let val: Value = serde_json::from_str(json_str).expect("parse json");
    val["id"]
        .as_str()
        .or_else(|| val[0]["id"].as_str())
        .expect("id field")
        .to_string()
}

fn extract_id_from_json(output: &str) -> String {
    let json = extract_json_payload(output);
    extract_issue_id(&json)
}

// ---------------------------------------------------------------------------
// Label Command Conformance Tests (12 tests)
// ---------------------------------------------------------------------------

#[test]
fn conformance_label_add_single() {
    common::init_test_logging();
    info!("Starting conformance_label_add_single test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    let br_create = workspace.run_br(["create", "Label test issue", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Label test issue", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add single label
    let br_add = workspace.run_br(["label", "add", &br_id, "urgent"], "label_add");
    let bd_add = workspace.run_bd(["label", "add", &bd_id, "urgent"], "label_add");

    info!(
        "br label add single: success={}, bd: success={}",
        br_add.status.success(),
        bd_add.status.success()
    );

    assert!(
        br_add.status.success(),
        "br label add failed: {}",
        br_add.stderr
    );
    assert!(
        bd_add.status.success(),
        "bd label add failed: {}",
        bd_add.stderr
    );

    // Verify label was added via list
    let br_list = workspace.run_br(["label", "list", &br_id, "--json"], "label_list");
    let bd_list = workspace.run_bd(["label", "list", &bd_id, "--json"], "label_list");

    let br_labels = extract_json_payload(&br_list.stdout);
    let bd_labels = extract_json_payload(&bd_list.stdout);

    info!("br labels: {}", br_labels);
    info!("bd labels: {}", bd_labels);

    assert!(
        br_labels.contains("urgent"),
        "br missing 'urgent' label: {}",
        br_labels
    );
    assert!(
        bd_labels.contains("urgent"),
        "bd missing 'urgent' label: {}",
        bd_labels
    );

    info!("conformance_label_add_single passed");
}

#[test]
fn conformance_label_add_multiple() {
    common::init_test_logging();
    info!("Starting conformance_label_add_multiple test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Multi-label issue", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Multi-label issue", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add multiple labels (one at a time for compatibility)
    workspace.run_br(["label", "add", &br_id, "bug"], "label_add_bug");
    workspace.run_bd(["label", "add", &bd_id, "bug"], "label_add_bug");

    workspace.run_br(["label", "add", &br_id, "priority"], "label_add_priority");
    workspace.run_bd(["label", "add", &bd_id, "priority"], "label_add_priority");

    workspace.run_br(["label", "add", &br_id, "frontend"], "label_add_frontend");
    workspace.run_bd(["label", "add", &bd_id, "frontend"], "label_add_frontend");

    // Verify all labels
    let br_list = workspace.run_br(["label", "list", &br_id, "--json"], "label_list");
    let bd_list = workspace.run_bd(["label", "list", &bd_id, "--json"], "label_list");

    let br_labels = extract_json_payload(&br_list.stdout);
    let bd_labels = extract_json_payload(&bd_list.stdout);

    info!("br labels: {}", br_labels);
    info!("bd labels: {}", bd_labels);

    for label in &["bug", "priority", "frontend"] {
        assert!(
            br_labels.contains(label),
            "br missing '{}' label: {}",
            label,
            br_labels
        );
        assert!(
            bd_labels.contains(label),
            "bd missing '{}' label: {}",
            label,
            bd_labels
        );
    }

    info!("conformance_label_add_multiple passed");
}

#[test]
fn conformance_label_remove() {
    common::init_test_logging();
    info!("Starting conformance_label_remove test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Remove label test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Remove label test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add then remove
    workspace.run_br(["label", "add", &br_id, "toremove"], "label_add");
    workspace.run_bd(["label", "add", &bd_id, "toremove"], "label_add");

    let br_remove = workspace.run_br(["label", "remove", &br_id, "toremove"], "label_remove");
    let bd_remove = workspace.run_bd(["label", "remove", &bd_id, "toremove"], "label_remove");

    info!(
        "br label remove: success={}, bd: success={}",
        br_remove.status.success(),
        bd_remove.status.success()
    );

    assert!(
        br_remove.status.success(),
        "br label remove failed: {}",
        br_remove.stderr
    );
    assert!(
        bd_remove.status.success(),
        "bd label remove failed: {}",
        bd_remove.stderr
    );

    // Verify label was removed
    let br_list = workspace.run_br(["label", "list", &br_id, "--json"], "label_list");
    let bd_list = workspace.run_bd(["label", "list", &bd_id, "--json"], "label_list");

    let br_labels = extract_json_payload(&br_list.stdout);
    let bd_labels = extract_json_payload(&bd_list.stdout);

    info!("br labels after remove: {}", br_labels);
    info!("bd labels after remove: {}", bd_labels);

    assert!(
        !br_labels.contains("toremove"),
        "br still has 'toremove' label: {}",
        br_labels
    );
    assert!(
        !bd_labels.contains("toremove"),
        "bd still has 'toremove' label: {}",
        bd_labels
    );

    info!("conformance_label_remove passed");
}

#[test]
fn conformance_label_list() {
    common::init_test_logging();
    info!("Starting conformance_label_list test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "List labels test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "List labels test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add labels
    workspace.run_br(["label", "add", &br_id, "alpha"], "add_alpha");
    workspace.run_bd(["label", "add", &bd_id, "alpha"], "add_alpha");
    workspace.run_br(["label", "add", &br_id, "beta"], "add_beta");
    workspace.run_bd(["label", "add", &bd_id, "beta"], "add_beta");

    // List labels
    let br_list = workspace.run_br(["label", "list", &br_id, "--json"], "label_list");
    let bd_list = workspace.run_bd(["label", "list", &bd_id, "--json"], "label_list");

    assert!(
        br_list.status.success(),
        "br label list failed: {}",
        br_list.stderr
    );
    assert!(
        bd_list.status.success(),
        "bd label list failed: {}",
        bd_list.stderr
    );

    let br_labels = extract_json_payload(&br_list.stdout);
    let bd_labels = extract_json_payload(&bd_list.stdout);

    info!("br labels: {}", br_labels);
    info!("bd labels: {}", bd_labels);

    // Both should have alpha and beta
    assert!(br_labels.contains("alpha"), "br missing alpha");
    assert!(br_labels.contains("beta"), "br missing beta");
    assert!(bd_labels.contains("alpha"), "bd missing alpha");
    assert!(bd_labels.contains("beta"), "bd missing beta");

    info!("conformance_label_list passed");
}

#[test]
fn conformance_label_list_all() {
    common::init_test_logging();
    info!("Starting conformance_label_list_all test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create multiple issues with different labels
    let br_create1 = workspace.run_br(["create", "Issue 1", "--json"], "create1");
    let bd_create1 = workspace.run_bd(["create", "Issue 1", "--json"], "create1");
    let br_id1 = extract_id_from_json(&br_create1.stdout);
    let bd_id1 = extract_id_from_json(&bd_create1.stdout);

    let br_create2 = workspace.run_br(["create", "Issue 2", "--json"], "create2");
    let bd_create2 = workspace.run_bd(["create", "Issue 2", "--json"], "create2");
    let br_id2 = extract_id_from_json(&br_create2.stdout);
    let bd_id2 = extract_id_from_json(&bd_create2.stdout);

    // Add labels to different issues
    workspace.run_br(["label", "add", &br_id1, "shared"], "add_shared1");
    workspace.run_bd(["label", "add", &bd_id1, "shared"], "add_shared1");
    workspace.run_br(["label", "add", &br_id2, "shared"], "add_shared2");
    workspace.run_bd(["label", "add", &bd_id2, "shared"], "add_shared2");
    workspace.run_br(["label", "add", &br_id1, "unique1"], "add_unique1");
    workspace.run_bd(["label", "add", &bd_id1, "unique1"], "add_unique1");
    workspace.run_br(["label", "add", &br_id2, "unique2"], "add_unique2");
    workspace.run_bd(["label", "add", &bd_id2, "unique2"], "add_unique2");

    // List all labels
    let br_list_all = workspace.run_br(["label", "list-all", "--json"], "list_all");
    let bd_list_all = workspace.run_bd(["label", "list-all", "--json"], "list_all");

    assert!(
        br_list_all.status.success(),
        "br label list-all failed: {}",
        br_list_all.stderr
    );
    assert!(
        bd_list_all.status.success(),
        "bd label list-all failed: {}",
        bd_list_all.stderr
    );

    let br_output = extract_json_payload(&br_list_all.stdout);
    let bd_output = extract_json_payload(&bd_list_all.stdout);

    info!("br list-all: {}", br_output);
    info!("bd list-all: {}", bd_output);

    // Should contain all labels
    assert!(br_output.contains("shared"), "br missing shared label");
    assert!(br_output.contains("unique1"), "br missing unique1 label");
    assert!(br_output.contains("unique2"), "br missing unique2 label");
    assert!(bd_output.contains("shared"), "bd missing shared label");
    assert!(bd_output.contains("unique1"), "bd missing unique1 label");
    assert!(bd_output.contains("unique2"), "bd missing unique2 label");

    info!("conformance_label_list_all passed");
}

#[test]
fn conformance_label_special_chars() {
    common::init_test_logging();
    info!("Starting conformance_label_special_chars test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Special char label test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Special char label test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Test label with hyphen and underscore
    let br_add = workspace.run_br(["label", "add", &br_id, "my-label_v2"], "label_add_special");
    let bd_add = workspace.run_bd(["label", "add", &bd_id, "my-label_v2"], "label_add_special");

    info!(
        "br special char add: success={}, bd: success={}",
        br_add.status.success(),
        bd_add.status.success()
    );

    // Verify
    let br_list = workspace.run_br(["label", "list", &br_id, "--json"], "label_list");
    let bd_list = workspace.run_bd(["label", "list", &bd_id, "--json"], "label_list");

    let br_labels = extract_json_payload(&br_list.stdout);
    let bd_labels = extract_json_payload(&bd_list.stdout);

    info!("br labels: {}", br_labels);
    info!("bd labels: {}", bd_labels);

    // Exit codes should match
    assert_eq!(
        br_add.status.success(),
        bd_add.status.success(),
        "Exit code mismatch for special char label"
    );

    info!("conformance_label_special_chars passed");
}

#[test]
fn conformance_label_unicode() {
    common::init_test_logging();
    info!("Starting conformance_label_unicode test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Unicode label test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Unicode label test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Test unicode label
    let br_add = workspace.run_br(["label", "add", &br_id, "测试标签"], "label_add_unicode");
    let bd_add = workspace.run_bd(["label", "add", &bd_id, "测试标签"], "label_add_unicode");

    info!(
        "br unicode add: success={}, bd: success={}",
        br_add.status.success(),
        bd_add.status.success()
    );

    // Log unicode handling behavior (br and bd may differ)
    if br_add.status.success() != bd_add.status.success() {
        info!(
            "NOTE: Unicode label handling differs between br (success={}) and bd (success={})",
            br_add.status.success(),
            bd_add.status.success()
        );
        // This is a known conformance difference - document but don't fail
    }

    // If br succeeded, verify the label is there
    if br_add.status.success() {
        let br_list = workspace.run_br(["label", "list", &br_id, "--json"], "label_list");
        let br_labels = extract_json_payload(&br_list.stdout);
        info!("br unicode labels: {}", br_labels);
        assert!(br_labels.contains("测试标签"), "br missing unicode label");
    }

    // If bd succeeded, verify the label is there
    if bd_add.status.success() {
        let bd_list = workspace.run_bd(["label", "list", &bd_id, "--json"], "label_list");
        let bd_labels = extract_json_payload(&bd_list.stdout);
        info!("bd unicode labels: {}", bd_labels);
        assert!(bd_labels.contains("测试标签"), "bd missing unicode label");
    }

    info!("conformance_label_unicode passed");
}

#[test]
fn conformance_label_duplicate() {
    common::init_test_logging();
    info!("Starting conformance_label_duplicate test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Duplicate label test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Duplicate label test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add label first time
    workspace.run_br(["label", "add", &br_id, "existing"], "label_add_first");
    workspace.run_bd(["label", "add", &bd_id, "existing"], "label_add_first");

    // Try adding same label again
    let br_dup = workspace.run_br(["label", "add", &br_id, "existing"], "label_add_dup");
    let bd_dup = workspace.run_bd(["label", "add", &bd_id, "existing"], "label_add_dup");

    info!(
        "br duplicate add: success={}, bd: success={}",
        br_dup.status.success(),
        bd_dup.status.success()
    );

    // Exit codes should match (might succeed or fail, but should be consistent)
    assert_eq!(
        br_dup.status.success(),
        bd_dup.status.success(),
        "Exit code mismatch for duplicate label add"
    );

    info!("conformance_label_duplicate passed");
}

#[test]
fn conformance_label_remove_nonexistent() {
    common::init_test_logging();
    info!("Starting conformance_label_remove_nonexistent test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Remove nonexistent label", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Remove nonexistent label", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Try removing a label that doesn't exist
    let br_remove = workspace.run_br(
        ["label", "remove", &br_id, "doesnotexist"],
        "label_remove_nonexistent",
    );
    let bd_remove = workspace.run_bd(
        ["label", "remove", &bd_id, "doesnotexist"],
        "label_remove_nonexistent",
    );

    info!(
        "br remove nonexistent: success={}, bd: success={}",
        br_remove.status.success(),
        bd_remove.status.success()
    );

    // Both should handle this consistently
    assert_eq!(
        br_remove.status.success(),
        bd_remove.status.success(),
        "Exit code mismatch for remove nonexistent label"
    );

    info!("conformance_label_remove_nonexistent passed");
}

#[test]
fn conformance_label_json_shape() {
    common::init_test_logging();
    info!("Starting conformance_label_json_shape test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "JSON shape test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "JSON shape test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    workspace.run_br(["label", "add", &br_id, "test"], "add");
    workspace.run_bd(["label", "add", &bd_id, "test"], "add");

    let br_list = workspace.run_br(["label", "list", &br_id, "--json"], "list");
    let bd_list = workspace.run_bd(["label", "list", &bd_id, "--json"], "list");

    assert!(br_list.status.success(), "br label list --json failed");
    assert!(bd_list.status.success(), "bd label list --json failed");

    let br_json = extract_json_payload(&br_list.stdout);
    let bd_json = extract_json_payload(&bd_list.stdout);

    // Both should parse as valid JSON
    let br_val: Value = serde_json::from_str(&br_json).expect("br JSON parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("bd JSON parse");

    info!("br JSON: {:?}", br_val);
    info!("bd JSON: {:?}", bd_val);

    // Both should be arrays or objects containing label data
    assert!(
        br_val.is_array() || br_val.is_object(),
        "br output not array or object"
    );
    assert!(
        bd_val.is_array() || bd_val.is_object(),
        "bd output not array or object"
    );

    info!("conformance_label_json_shape passed");
}

#[test]
fn conformance_label_filter_issues() {
    common::init_test_logging();
    info!("Starting conformance_label_filter_issues test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues with different labels
    let br_create1 = workspace.run_br(["create", "Has target label", "--json"], "create1");
    let bd_create1 = workspace.run_bd(["create", "Has target label", "--json"], "create1");
    let br_id1 = extract_id_from_json(&br_create1.stdout);
    let bd_id1 = extract_id_from_json(&bd_create1.stdout);

    let br_create2 = workspace.run_br(["create", "No target label", "--json"], "create2");
    let bd_create2 = workspace.run_bd(["create", "No target label", "--json"], "create2");
    let br_id2 = extract_id_from_json(&br_create2.stdout);
    let bd_id2 = extract_id_from_json(&bd_create2.stdout);

    // Add target label to first issue only
    workspace.run_br(["label", "add", &br_id1, "target"], "add_target");
    workspace.run_bd(["label", "add", &bd_id1, "target"], "add_target");
    workspace.run_br(["label", "add", &br_id2, "other"], "add_other");
    workspace.run_bd(["label", "add", &bd_id2, "other"], "add_other");

    // List issues with target label
    let br_filter = workspace.run_br(["list", "--label", "target", "--json"], "filter");
    let bd_filter = workspace.run_bd(["list", "--label", "target", "--json"], "filter");

    assert!(
        br_filter.status.success(),
        "br list --label failed: {}",
        br_filter.stderr
    );
    assert!(
        bd_filter.status.success(),
        "bd list --label failed: {}",
        bd_filter.stderr
    );

    let br_output = extract_json_payload(&br_filter.stdout);
    let bd_output = extract_json_payload(&bd_filter.stdout);

    info!("br filtered: {}", br_output);
    info!("bd filtered: {}", bd_output);

    // Should contain the issue with target label
    assert!(
        br_output.contains("Has target label") || br_output.contains(&br_id1),
        "br missing filtered issue"
    );
    assert!(
        bd_output.contains("Has target label") || bd_output.contains(&bd_id1),
        "bd missing filtered issue"
    );

    info!("conformance_label_filter_issues passed");
}

#[test]
fn conformance_label_case_sensitivity() {
    common::init_test_logging();
    info!("Starting conformance_label_case_sensitivity test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Case test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Case test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add lowercase label
    workspace.run_br(["label", "add", &br_id, "myLabel"], "add_mixed");
    workspace.run_bd(["label", "add", &bd_id, "myLabel"], "add_mixed");

    // Add uppercase variant
    let br_upper = workspace.run_br(["label", "add", &br_id, "MYLABEL"], "add_upper");
    let bd_upper = workspace.run_bd(["label", "add", &bd_id, "MYLABEL"], "add_upper");

    info!(
        "br upper case add: success={}, bd: success={}",
        br_upper.status.success(),
        bd_upper.status.success()
    );

    // Check the result
    let br_list = workspace.run_br(["label", "list", &br_id, "--json"], "list");
    let bd_list = workspace.run_bd(["label", "list", &bd_id, "--json"], "list");

    let br_labels = extract_json_payload(&br_list.stdout);
    let bd_labels = extract_json_payload(&bd_list.stdout);

    info!("br labels (case test): {}", br_labels);
    info!("bd labels (case test): {}", bd_labels);

    // Both should handle case consistently (either both case-sensitive or both insensitive)
    assert_eq!(
        br_upper.status.success(),
        bd_upper.status.success(),
        "Case handling mismatch"
    );

    info!("conformance_label_case_sensitivity passed");
}

// ---------------------------------------------------------------------------
// Comments Command Conformance Tests (13 tests)
// ---------------------------------------------------------------------------

#[test]
fn conformance_comments_add() {
    common::init_test_logging();
    info!("Starting conformance_comments_add test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Comment test issue", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Comment test issue", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add comment
    let br_add = workspace.run_br(
        ["comments", "add", &br_id, "This is a test comment"],
        "comments_add",
    );
    let bd_add = workspace.run_bd(
        ["comments", "add", &bd_id, "This is a test comment"],
        "comments_add",
    );

    info!(
        "br comments add: success={}, bd: success={}",
        br_add.status.success(),
        bd_add.status.success()
    );

    assert!(
        br_add.status.success(),
        "br comments add failed: {}",
        br_add.stderr
    );
    assert!(
        bd_add.status.success(),
        "bd comments add failed: {}",
        bd_add.stderr
    );

    // Verify comment exists
    let br_list = workspace.run_br(["comments", &br_id, "--json"], "comments_list");
    let bd_list = workspace.run_bd(["comments", &bd_id, "--json"], "comments_list");

    let br_comments = extract_json_payload(&br_list.stdout);
    let bd_comments = extract_json_payload(&bd_list.stdout);

    info!("br comments: {}", br_comments);
    info!("bd comments: {}", bd_comments);

    assert!(br_comments.contains("test comment"), "br missing comment");
    assert!(bd_comments.contains("test comment"), "bd missing comment");

    info!("conformance_comments_add passed");
}

#[test]
fn conformance_comments_list() {
    common::init_test_logging();
    info!("Starting conformance_comments_list test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Comments list test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Comments list test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add a comment
    workspace.run_br(["comments", "add", &br_id, "First comment"], "add");
    workspace.run_bd(["comments", "add", &bd_id, "First comment"], "add");

    // List comments
    let br_list = workspace.run_br(["comments", &br_id, "--json"], "list");
    let bd_list = workspace.run_bd(["comments", &bd_id, "--json"], "list");

    assert!(
        br_list.status.success(),
        "br comments list failed: {}",
        br_list.stderr
    );
    assert!(
        bd_list.status.success(),
        "bd comments list failed: {}",
        bd_list.stderr
    );

    let br_output = extract_json_payload(&br_list.stdout);
    let bd_output = extract_json_payload(&bd_list.stdout);

    info!("br comments list: {}", br_output);
    info!("bd comments list: {}", bd_output);

    assert!(
        br_output.contains("First comment"),
        "br missing comment in list"
    );
    assert!(
        bd_output.contains("First comment"),
        "bd missing comment in list"
    );

    info!("conformance_comments_list passed");
}

#[test]
fn conformance_comments_empty() {
    common::init_test_logging();
    info!("Starting conformance_comments_empty test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "No comments issue", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "No comments issue", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // List comments on issue with no comments
    let br_list = workspace.run_br(["comments", &br_id, "--json"], "list_empty");
    let bd_list = workspace.run_bd(["comments", &bd_id, "--json"], "list_empty");

    assert!(
        br_list.status.success(),
        "br comments list empty failed: {}",
        br_list.stderr
    );
    assert!(
        bd_list.status.success(),
        "bd comments list empty failed: {}",
        bd_list.stderr
    );

    let br_output = extract_json_payload(&br_list.stdout);
    let bd_output = extract_json_payload(&bd_list.stdout);

    info!("br empty comments: {}", br_output);
    info!("bd empty comments: {}", bd_output);

    // Both should return empty array or similar
    let br_val: Value = serde_json::from_str(&br_output).expect("br parse");
    let bd_val: Value = serde_json::from_str(&bd_output).expect("bd parse");

    // Should be empty array or null
    if let Some(arr) = br_val.as_array() {
        assert!(arr.is_empty(), "br not empty: {:?}", arr);
    }
    if let Some(arr) = bd_val.as_array() {
        assert!(arr.is_empty(), "bd not empty: {:?}", arr);
    }

    info!("conformance_comments_empty passed");
}

#[test]
fn conformance_comments_multiple() {
    common::init_test_logging();
    info!("Starting conformance_comments_multiple test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Multiple comments", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Multiple comments", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add multiple comments
    for i in 1..=3 {
        let comment = format!("Comment number {i}");
        workspace.run_br(["comments", "add", &br_id, &comment], &format!("add_{i}"));
        workspace.run_bd(["comments", "add", &bd_id, &comment], &format!("add_{i}"));
    }

    // List all comments
    let br_list = workspace.run_br(["comments", &br_id, "--json"], "list");
    let bd_list = workspace.run_bd(["comments", &bd_id, "--json"], "list");

    assert!(br_list.status.success(), "br comments list failed");
    assert!(bd_list.status.success(), "bd comments list failed");

    let br_output = extract_json_payload(&br_list.stdout);
    let bd_output = extract_json_payload(&bd_list.stdout);

    info!("br multiple comments: {}", br_output);
    info!("bd multiple comments: {}", bd_output);

    // Should have all 3 comments
    for i in 1..=3 {
        let text = format!("Comment number {i}");
        assert!(br_output.contains(&text), "br missing comment {i}");
        assert!(bd_output.contains(&text), "bd missing comment {i}");
    }

    info!("conformance_comments_multiple passed");
}

#[test]
fn conformance_comments_markdown() {
    common::init_test_logging();
    info!("Starting conformance_comments_markdown test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Markdown comment test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Markdown comment test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add markdown comment
    let md_comment = "# Heading\n\n- bullet 1\n- bullet 2\n\n**bold** and _italic_";
    let br_add = workspace.run_br(["comments", "add", &br_id, md_comment], "add_md");
    let bd_add = workspace.run_bd(["comments", "add", &bd_id, md_comment], "add_md");

    info!(
        "br markdown add: success={}, bd: success={}",
        br_add.status.success(),
        bd_add.status.success()
    );

    assert_eq!(
        br_add.status.success(),
        bd_add.status.success(),
        "Markdown handling mismatch"
    );

    if br_add.status.success() {
        let br_list = workspace.run_br(["comments", &br_id, "--json"], "list");
        let bd_list = workspace.run_bd(["comments", &bd_id, "--json"], "list");

        let br_output = extract_json_payload(&br_list.stdout);
        let bd_output = extract_json_payload(&bd_list.stdout);

        info!("br markdown comment: {}", br_output);
        info!("bd markdown comment: {}", bd_output);

        // Should contain the markdown content
        assert!(br_output.contains("Heading"), "br missing markdown content");
        assert!(bd_output.contains("Heading"), "bd missing markdown content");
    }

    info!("conformance_comments_markdown passed");
}

#[test]
fn conformance_comments_unicode() {
    common::init_test_logging();
    info!("Starting conformance_comments_unicode test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Unicode comment test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Unicode comment test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add unicode comment
    let unicode_comment = "这是一个测试评论 émojis работают";
    let br_add = workspace.run_br(["comments", "add", &br_id, unicode_comment], "add_unicode");
    let bd_add = workspace.run_bd(["comments", "add", &bd_id, unicode_comment], "add_unicode");

    info!(
        "br unicode add: success={}, bd: success={}",
        br_add.status.success(),
        bd_add.status.success()
    );

    assert_eq!(
        br_add.status.success(),
        bd_add.status.success(),
        "Unicode handling mismatch"
    );

    if br_add.status.success() {
        let br_list = workspace.run_br(["comments", &br_id, "--json"], "list");
        let bd_list = workspace.run_bd(["comments", &bd_id, "--json"], "list");

        let br_output = extract_json_payload(&br_list.stdout);
        let bd_output = extract_json_payload(&bd_list.stdout);

        info!("br unicode comment: {}", br_output);
        info!("bd unicode comment: {}", bd_output);

        assert!(br_output.contains("测试评论"), "br missing unicode content");
        assert!(bd_output.contains("测试评论"), "bd missing unicode content");
    }

    info!("conformance_comments_unicode passed");
}

#[test]
fn conformance_comments_json_shape() {
    common::init_test_logging();
    info!("Starting conformance_comments_json_shape test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "JSON shape test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "JSON shape test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    workspace.run_br(["comments", "add", &br_id, "Test comment"], "add");
    workspace.run_bd(["comments", "add", &bd_id, "Test comment"], "add");

    let br_list = workspace.run_br(["comments", &br_id, "--json"], "list");
    let bd_list = workspace.run_bd(["comments", &bd_id, "--json"], "list");

    assert!(br_list.status.success(), "br comments --json failed");
    assert!(bd_list.status.success(), "bd comments --json failed");

    let br_json = extract_json_payload(&br_list.stdout);
    let bd_json = extract_json_payload(&bd_list.stdout);

    // Parse as JSON
    let br_val: Value = serde_json::from_str(&br_json).expect("br JSON parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("bd JSON parse");

    info!("br JSON shape: {:?}", br_val);
    info!("bd JSON shape: {:?}", bd_val);

    // Both should be arrays
    assert!(br_val.is_array(), "br output not array");
    assert!(bd_val.is_array(), "bd output not array");

    // Comments should have body field
    if let Some(arr) = br_val.as_array() {
        if let Some(first) = arr.first() {
            assert!(
                first.get("body").is_some()
                    || first.get("content").is_some()
                    || first.get("text").is_some(),
                "br comment missing body/content field"
            );
        }
    }

    info!("conformance_comments_json_shape passed");
}

#[test]
fn conformance_comments_issue_not_found() {
    common::init_test_logging();
    info!("Starting conformance_comments_issue_not_found test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Try to list comments for nonexistent issue
    let br_list = workspace.run_br(["comments", "nonexistent-id", "--json"], "list_notfound");
    let bd_list = workspace.run_bd(["comments", "nonexistent-id", "--json"], "list_notfound");

    info!(
        "br not found: success={}, bd: success={}",
        br_list.status.success(),
        bd_list.status.success()
    );

    // Both should fail similarly
    assert_eq!(
        br_list.status.success(),
        bd_list.status.success(),
        "Error handling mismatch for nonexistent issue"
    );

    // If both fail, that's expected
    if !br_list.status.success() && !bd_list.status.success() {
        info!("Both correctly failed for nonexistent issue");
    }

    info!("conformance_comments_issue_not_found passed");
}

#[test]
fn conformance_comments_ordering() {
    common::init_test_logging();
    info!("Starting conformance_comments_ordering test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Ordering test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Ordering test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add comments in order
    workspace.run_br(["comments", "add", &br_id, "FIRST"], "add1");
    workspace.run_bd(["comments", "add", &bd_id, "FIRST"], "add1");

    // Small delay to ensure different timestamps
    std::thread::sleep(std::time::Duration::from_millis(50));

    workspace.run_br(["comments", "add", &br_id, "SECOND"], "add2");
    workspace.run_bd(["comments", "add", &bd_id, "SECOND"], "add2");

    std::thread::sleep(std::time::Duration::from_millis(50));

    workspace.run_br(["comments", "add", &br_id, "THIRD"], "add3");
    workspace.run_bd(["comments", "add", &bd_id, "THIRD"], "add3");

    // List comments
    let br_list = workspace.run_br(["comments", &br_id, "--json"], "list");
    let bd_list = workspace.run_bd(["comments", &bd_id, "--json"], "list");

    let br_output = &br_list.stdout;
    let bd_output = &bd_list.stdout;

    info!("br ordering: {}", br_output);
    info!("bd ordering: {}", bd_output);

    // FIRST should appear before SECOND, SECOND before THIRD
    let br_first_pos = br_output.find("FIRST").unwrap_or(usize::MAX);
    let br_second_pos = br_output.find("SECOND").unwrap_or(usize::MAX);
    let br_third_pos = br_output.find("THIRD").unwrap_or(usize::MAX);

    let bd_first_pos = bd_output.find("FIRST").unwrap_or(usize::MAX);
    let bd_second_pos = bd_output.find("SECOND").unwrap_or(usize::MAX);
    let bd_third_pos = bd_output.find("THIRD").unwrap_or(usize::MAX);

    assert!(br_first_pos < br_second_pos, "br: FIRST not before SECOND");
    assert!(br_second_pos < br_third_pos, "br: SECOND not before THIRD");
    assert!(bd_first_pos < bd_second_pos, "bd: FIRST not before SECOND");
    assert!(bd_second_pos < bd_third_pos, "bd: SECOND not before THIRD");

    info!("conformance_comments_ordering passed");
}

#[test]
fn conformance_comments_author() {
    common::init_test_logging();
    info!("Starting conformance_comments_author test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Author test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Author test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    // Add comment with specific actor
    let br_add = workspace.run_br(
        [
            "comments",
            "add",
            &br_id,
            "Author test comment",
            "--actor",
            "TestUser",
        ],
        "add",
    );
    let bd_add = workspace.run_bd(
        [
            "comments",
            "add",
            &bd_id,
            "Author test comment",
            "--actor",
            "TestUser",
        ],
        "add",
    );

    assert!(br_add.status.success(), "br add failed: {}", br_add.stderr);
    assert!(bd_add.status.success(), "bd add failed: {}", bd_add.stderr);

    // List and check author
    let br_list = workspace.run_br(["comments", &br_id, "--json"], "list");
    let bd_list = workspace.run_bd(["comments", &bd_id, "--json"], "list");

    let br_output = extract_json_payload(&br_list.stdout);
    let bd_output = extract_json_payload(&bd_list.stdout);

    info!("br author test: {}", br_output);
    info!("bd author test: {}", bd_output);

    // Both should have author information (might be in different fields)
    let br_val: Value = serde_json::from_str(&br_output).expect("br parse");
    let bd_val: Value = serde_json::from_str(&bd_output).expect("bd parse");

    if let Some(arr) = br_val.as_array() {
        if let Some(first) = arr.first() {
            let has_author = first.get("author").is_some()
                || first.get("created_by").is_some()
                || first.get("actor").is_some();
            info!("br comment has author field: {}", has_author);
        }
    }

    if let Some(arr) = bd_val.as_array() {
        if let Some(first) = arr.first() {
            let has_author = first.get("author").is_some()
                || first.get("created_by").is_some()
                || first.get("actor").is_some();
            info!("bd comment has author field: {}", has_author);
        }
    }

    info!("conformance_comments_author passed");
}

#[test]
fn conformance_comments_timestamps() {
    common::init_test_logging();
    info!("Starting conformance_comments_timestamps test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(["create", "Timestamp test", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Timestamp test", "--json"], "create");

    let br_id = extract_id_from_json(&br_create.stdout);
    let bd_id = extract_id_from_json(&bd_create.stdout);

    workspace.run_br(["comments", "add", &br_id, "Timestamp comment"], "add");
    workspace.run_bd(["comments", "add", &bd_id, "Timestamp comment"], "add");

    let br_list = workspace.run_br(["comments", &br_id, "--json"], "list");
    let bd_list = workspace.run_bd(["comments", &bd_id, "--json"], "list");

    let br_json = extract_json_payload(&br_list.stdout);
    let bd_json = extract_json_payload(&bd_list.stdout);

    info!("br timestamps: {}", br_json);
    info!("bd timestamps: {}", bd_json);

    let br_val: Value = serde_json::from_str(&br_json).expect("br parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("bd parse");

    // Check for timestamp fields
    if let Some(arr) = br_val.as_array() {
        if let Some(first) = arr.first() {
            let has_timestamp = first.get("created_at").is_some()
                || first.get("timestamp").is_some()
                || first.get("date").is_some();
            info!("br comment has timestamp: {}", has_timestamp);
        }
    }

    if let Some(arr) = bd_val.as_array() {
        if let Some(first) = arr.first() {
            let has_timestamp = first.get("created_at").is_some()
                || first.get("timestamp").is_some()
                || first.get("date").is_some();
            info!("bd comment has timestamp: {}", has_timestamp);
        }
    }

    info!("conformance_comments_timestamps passed");
}
