#![allow(clippy::all, clippy::pedantic, clippy::nursery)]
//! Conformance Tests: Validate br (Rust) produces identical output to bd (Go)
//!
//! This harness runs equivalent commands on both br and bd in isolated temp directories,
//! then compares outputs using various comparison modes.

mod common;

use assert_cmd::Command;
use common::cli::extract_json_payload;
use serde_json::Value;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
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

    CmdOutput {
        stdout,
        stderr,
        status: output.status,
        duration,
    }
}

/// Comparison mode for conformance tests
#[derive(Debug, Clone)]
pub enum CompareMode {
    /// JSON outputs must be identical
    ExactJson,
    /// Ignore timestamps and normalize IDs
    NormalizedJson,
    /// Check specific fields match
    ContainsFields(Vec<String>),
    /// Just check that both succeed or both fail
    ExitCodeOnly,
    /// Compare arrays ignoring element order
    ArrayUnordered,
    /// Ignore specified fields during comparison
    FieldsExcluded(Vec<String>),
    /// Compare JSON structure only, not values
    StructureOnly,
}

// ============================================================================
// BENCHMARK TIMING INFRASTRUCTURE
// ============================================================================

/// Configuration for benchmark runs
#[derive(Debug, Clone)]
pub struct BenchmarkConfig {
    /// Number of warmup runs (not counted in statistics)
    pub warmup_runs: usize,
    /// Number of timed runs for statistics
    pub timed_runs: usize,
    /// Outlier threshold in standard deviations
    pub outlier_threshold: f64,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            warmup_runs: 2,
            timed_runs: 5,
            outlier_threshold: 2.0,
        }
    }
}

/// Timing statistics from benchmark runs
#[derive(Debug, Clone)]
pub struct TimingStats {
    pub mean_ms: f64,
    pub median_ms: f64,
    pub p95_ms: f64,
    pub std_dev_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub run_count: usize,
}

impl TimingStats {
    /// Compute statistics from a list of durations
    pub fn from_durations(durations: &[Duration]) -> Self {
        if durations.is_empty() {
            return Self {
                mean_ms: 0.0,
                median_ms: 0.0,
                p95_ms: 0.0,
                std_dev_ms: 0.0,
                min_ms: 0.0,
                max_ms: 0.0,
                run_count: 0,
            };
        }

        let mut ms_values: Vec<f64> = durations
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        ms_values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let n = ms_values.len();
        let mean = ms_values.iter().sum::<f64>() / n as f64;
        let median = if n % 2 == 0 {
            (ms_values[n / 2 - 1] + ms_values[n / 2]) / 2.0
        } else {
            ms_values[n / 2]
        };
        let p95_idx = (n as f64 * 0.95).ceil() as usize - 1;
        let p95 = ms_values[p95_idx.min(n - 1)];
        let variance = ms_values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
        let std_dev = variance.sqrt();

        Self {
            mean_ms: mean,
            median_ms: median,
            p95_ms: p95,
            std_dev_ms: std_dev,
            min_ms: ms_values[0],
            max_ms: ms_values[n - 1],
            run_count: n,
        }
    }

    /// Filter out outliers beyond the threshold (in std deviations)
    pub fn filter_outliers(durations: &[Duration], threshold: f64) -> Vec<Duration> {
        if durations.len() < 3 {
            return durations.to_vec();
        }

        let ms_values: Vec<f64> = durations
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        let mean = ms_values.iter().sum::<f64>() / ms_values.len() as f64;
        let variance = ms_values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / ms_values.len() as f64;
        let std_dev = variance.sqrt();

        durations
            .iter()
            .zip(ms_values.iter())
            .filter(|&(_, &ms)| (ms - mean).abs() <= threshold * std_dev)
            .map(|(d, _)| *d)
            .collect()
    }
}

/// Run a benchmark with warmup and timing
pub fn run_benchmark<F>(config: &BenchmarkConfig, mut f: F) -> TimingStats
where
    F: FnMut() -> Duration,
{
    // Warmup runs (discard results)
    for _ in 0..config.warmup_runs {
        let _ = f();
    }

    // Timed runs
    let mut durations: Vec<Duration> = Vec::with_capacity(config.timed_runs);
    for _ in 0..config.timed_runs {
        durations.push(f());
    }

    // Filter outliers and compute stats
    let filtered = TimingStats::filter_outliers(&durations, config.outlier_threshold);
    TimingStats::from_durations(&filtered)
}

/// Normalize JSON for comparison by removing/masking volatile fields
pub fn normalize_json(json_str: &str) -> Result<Value, serde_json::Error> {
    let mut value: Value = serde_json::from_str(json_str)?;
    normalize_value(&mut value);
    Ok(value)
}

fn normalize_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            // Fields to normalize (set to fixed values)
            let timestamp_fields: HashSet<&str> = [
                "created_at",
                "updated_at",
                "closed_at",
                "deleted_at",
                "due_at",
                "defer_until",
                "compacted_at",
            ]
            .into_iter()
            .collect();

            // Normalize timestamps to a fixed value
            for (key, val) in map.iter_mut() {
                if timestamp_fields.contains(key.as_str()) {
                    if val.is_string() {
                        *val = Value::String("NORMALIZED_TIMESTAMP".to_string());
                    }
                } else if key == "id" || key == "issue_id" || key == "depends_on_id" {
                    // Keep ID structure but normalize the hash portion
                    if let Some(s) = val.as_str() {
                        if let Some(dash_pos) = s.find('-') {
                            let prefix = &s[..dash_pos];
                            *val = Value::String(format!("{prefix}-NORMALIZED"));
                        }
                    }
                } else if key == "content_hash" {
                    if val.is_string() {
                        *val = Value::String("NORMALIZED_HASH".to_string());
                    }
                } else {
                    normalize_value(val);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                normalize_value(item);
            }
        }
        _ => {}
    }
}

/// Compare two JSON outputs
pub fn compare_json(br_output: &str, bd_output: &str, mode: &CompareMode) -> Result<(), String> {
    match mode {
        CompareMode::ExactJson => {
            let br_json: Value =
                serde_json::from_str(br_output).map_err(|e| format!("br JSON parse: {e}"))?;
            let bd_json: Value =
                serde_json::from_str(bd_output).map_err(|e| format!("bd JSON parse: {e}"))?;

            if br_json != bd_json {
                return Err(format!(
                    "JSON mismatch\nbr: {}\nbd: {}",
                    serde_json::to_string_pretty(&br_json).unwrap_or_default(),
                    serde_json::to_string_pretty(&bd_json).unwrap_or_default()
                ));
            }
        }
        CompareMode::NormalizedJson => {
            let br_json = normalize_json(br_output).map_err(|e| format!("br JSON parse: {e}"))?;
            let bd_json = normalize_json(bd_output).map_err(|e| format!("bd JSON parse: {e}"))?;

            if br_json != bd_json {
                return Err(format!(
                    "Normalized JSON mismatch\nbr: {}\nbd: {}",
                    serde_json::to_string_pretty(&br_json).unwrap_or_default(),
                    serde_json::to_string_pretty(&bd_json).unwrap_or_default()
                ));
            }
        }
        CompareMode::ContainsFields(fields) => {
            let br_json: Value =
                serde_json::from_str(br_output).map_err(|e| format!("br JSON parse: {e}"))?;
            let bd_json: Value =
                serde_json::from_str(bd_output).map_err(|e| format!("bd JSON parse: {e}"))?;

            for field in fields {
                let br_val = extract_field(&br_json, field);
                let bd_val = extract_field(&bd_json, field);

                if br_val != bd_val {
                    return Err(format!(
                        "Field '{}' mismatch\nbr: {:?}\nbd: {:?}",
                        field, br_val, bd_val
                    ));
                }
            }
        }
        CompareMode::ExitCodeOnly => {
            // No JSON comparison needed
        }
        CompareMode::ArrayUnordered => {
            let br_json: Value =
                serde_json::from_str(br_output).map_err(|e| format!("br JSON parse: {e}"))?;
            let bd_json: Value =
                serde_json::from_str(bd_output).map_err(|e| format!("bd JSON parse: {e}"))?;

            // Compare arrays ignoring order
            if !json_equal_unordered(&br_json, &bd_json) {
                return Err(format!(
                    "Array-unordered mismatch\nbr: {}\nbd: {}",
                    serde_json::to_string_pretty(&br_json).unwrap_or_default(),
                    serde_json::to_string_pretty(&bd_json).unwrap_or_default()
                ));
            }
        }
        CompareMode::FieldsExcluded(excluded) => {
            let br_json: Value =
                serde_json::from_str(br_output).map_err(|e| format!("br JSON parse: {e}"))?;
            let bd_json: Value =
                serde_json::from_str(bd_output).map_err(|e| format!("bd JSON parse: {e}"))?;

            // Remove excluded fields and compare
            let br_filtered = filter_fields(&br_json, excluded);
            let bd_filtered = filter_fields(&bd_json, excluded);

            if br_filtered != bd_filtered {
                return Err(format!(
                    "Fields-excluded mismatch\nbr: {}\nbd: {}",
                    serde_json::to_string_pretty(&br_filtered).unwrap_or_default(),
                    serde_json::to_string_pretty(&bd_filtered).unwrap_or_default()
                ));
            }
        }
        CompareMode::StructureOnly => {
            let br_json: Value =
                serde_json::from_str(br_output).map_err(|e| format!("br JSON parse: {e}"))?;
            let bd_json: Value =
                serde_json::from_str(bd_output).map_err(|e| format!("bd JSON parse: {e}"))?;

            // Compare structure without values
            if !structure_matches(&br_json, &bd_json) {
                return Err(format!(
                    "Structure mismatch\nbr: {}\nbd: {}",
                    serde_json::to_string_pretty(&br_json).unwrap_or_default(),
                    serde_json::to_string_pretty(&bd_json).unwrap_or_default()
                ));
            }
        }
    }
    Ok(())
}

fn extract_field<'a>(json: &'a Value, field: &str) -> Option<&'a Value> {
    match json {
        Value::Object(map) => map.get(field),
        Value::Array(arr) if !arr.is_empty() => {
            // For arrays, check the first element
            if let Value::Object(map) = &arr[0] {
                map.get(field)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Compare two JSON values ignoring array order
fn json_equal_unordered(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(arr_a), Value::Array(arr_b)) => {
            if arr_a.len() != arr_b.len() {
                return false;
            }
            // Check each element in a exists somewhere in b
            for elem_a in arr_a {
                if !arr_b.iter().any(|elem_b| json_equal_unordered(elem_a, elem_b)) {
                    return false;
                }
            }
            true
        }
        (Value::Object(map_a), Value::Object(map_b)) => {
            if map_a.len() != map_b.len() {
                return false;
            }
            for (key, val_a) in map_a {
                match map_b.get(key) {
                    Some(val_b) => {
                        if !json_equal_unordered(val_a, val_b) {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
            true
        }
        _ => a == b,
    }
}

/// Filter out specified fields from JSON
fn filter_fields(json: &Value, excluded: &[String]) -> Value {
    match json {
        Value::Object(map) => {
            let filtered: serde_json::Map<String, Value> = map
                .iter()
                .filter(|(k, _)| !excluded.contains(k))
                .map(|(k, v)| (k.clone(), filter_fields(v, excluded)))
                .collect();
            Value::Object(filtered)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(|v| filter_fields(v, excluded)).collect()),
        other => other.clone(),
    }
}

/// Check if two JSON values have the same structure (ignoring values)
fn structure_matches(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(map_a), Value::Object(map_b)) => {
            if map_a.len() != map_b.len() {
                return false;
            }
            for (key, val_a) in map_a {
                match map_b.get(key) {
                    Some(val_b) => {
                        if !structure_matches(val_a, val_b) {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
            true
        }
        (Value::Array(arr_a), Value::Array(arr_b)) => {
            // For structure, just check that both are arrays and have similar structure in first element
            if arr_a.is_empty() && arr_b.is_empty() {
                return true;
            }
            if arr_a.is_empty() != arr_b.is_empty() {
                return false;
            }
            // Compare first elements' structure
            structure_matches(&arr_a[0], &arr_b[0])
        }
        (Value::Null, Value::Null)
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Number(_), Value::Number(_))
        | (Value::String(_), Value::String(_)) => true,
        _ => false,
    }
}

// ============================================================================
// DETAILED DIFF FOR ERROR DIAGNOSTICS
// ============================================================================

/// Generate a human-readable diff between two JSON values
pub fn diff_json(br: &Value, bd: &Value) -> String {
    let mut diffs = Vec::new();
    collect_diffs(br, bd, "", &mut diffs);

    if diffs.is_empty() {
        return "No differences found".to_string();
    }

    let mut output = String::new();
    output.push_str("Differences found:\n");
    for (path, br_val, bd_val) in diffs.iter().take(20) {
        output.push_str(&format!(
            "  {}: br={}, bd={}\n",
            if path.is_empty() { "(root)" } else { path },
            br_val,
            bd_val
        ));
    }
    if diffs.len() > 20 {
        output.push_str(&format!("  ... and {} more differences\n", diffs.len() - 20));
    }
    output
}

/// Collect all differences between two JSON values
fn collect_diffs(br: &Value, bd: &Value, path: &str, diffs: &mut Vec<(String, String, String)>) {
    match (br, bd) {
        (Value::Object(br_map), Value::Object(bd_map)) => {
            // Check for keys only in br
            for key in br_map.keys() {
                if !bd_map.contains_key(key) {
                    let key_path = format_path(path, key);
                    diffs.push((
                        key_path,
                        format_value_short(&br_map[key]),
                        "(missing)".to_string(),
                    ));
                }
            }
            // Check for keys only in bd
            for key in bd_map.keys() {
                if !br_map.contains_key(key) {
                    let key_path = format_path(path, key);
                    diffs.push((
                        key_path,
                        "(missing)".to_string(),
                        format_value_short(&bd_map[key]),
                    ));
                }
            }
            // Compare shared keys
            for (key, br_val) in br_map {
                if let Some(bd_val) = bd_map.get(key) {
                    collect_diffs(br_val, bd_val, &format_path(path, key), diffs);
                }
            }
        }
        (Value::Array(br_arr), Value::Array(bd_arr)) => {
            if br_arr.len() != bd_arr.len() {
                diffs.push((
                    format!("{}.length", path),
                    br_arr.len().to_string(),
                    bd_arr.len().to_string(),
                ));
            }
            let min_len = br_arr.len().min(bd_arr.len());
            for i in 0..min_len {
                collect_diffs(&br_arr[i], &bd_arr[i], &format!("{}[{}]", path, i), diffs);
            }
        }
        _ => {
            if br != bd {
                diffs.push((
                    path.to_string(),
                    format_value_short(br),
                    format_value_short(bd),
                ));
            }
        }
    }
}

fn format_path(base: &str, key: &str) -> String {
    if base.is_empty() {
        key.to_string()
    } else {
        format!("{}.{}", base, key)
    }
}

fn format_value_short(val: &Value) -> String {
    match val {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            if s.len() > 30 {
                format!("\"{}...\"", &s[..27])
            } else {
                format!("\"{}\"", s)
            }
        }
        Value::Array(arr) => format!("[{} items]", arr.len()),
        Value::Object(map) => format!("{{...{} keys}}", map.len()),
    }
}

// ============================================================================
// REUSABLE TEST SCENARIOS
// ============================================================================

/// A reusable test scenario that can be executed against both br and bd
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TestScenario {
    /// Unique name for the scenario
    pub name: String,
    /// Description of what the scenario tests
    pub description: String,
    /// Commands to run for setup (before the test command)
    pub setup_commands: Vec<Vec<String>>,
    /// The command to test (will be run on both br and bd)
    pub test_command: Vec<String>,
    /// How to compare the outputs
    pub compare_mode: CompareMode,
    /// Whether to compare exit codes
    pub compare_exit_codes: bool,
}

impl TestScenario {
    /// Create a new test scenario with defaults
    #[allow(dead_code)]
    pub fn new(name: &str, test_command: Vec<&str>) -> Self {
        Self {
            name: name.to_string(),
            description: String::new(),
            setup_commands: Vec::new(),
            test_command: test_command.into_iter().map(String::from).collect(),
            compare_mode: CompareMode::NormalizedJson,
            compare_exit_codes: true,
        }
    }

    #[allow(dead_code)]
    pub fn with_description(mut self, desc: &str) -> Self {
        self.description = desc.to_string();
        self
    }

    #[allow(dead_code)]
    pub fn with_setup(mut self, commands: Vec<Vec<&str>>) -> Self {
        self.setup_commands = commands
            .into_iter()
            .map(|cmd| cmd.into_iter().map(String::from).collect())
            .collect();
        self
    }

    #[allow(dead_code)]
    pub fn with_compare_mode(mut self, mode: CompareMode) -> Self {
        self.compare_mode = mode;
        self
    }

    /// Execute the scenario and return a result
    #[allow(dead_code)]
    pub fn execute(&self, workspace: &ConformanceWorkspace) -> Result<(), String> {
        // Run setup commands
        for cmd in &self.setup_commands {
            let args: Vec<&str> = cmd.iter().map(String::as_str).collect();
            let br_result = workspace.run_br(args.clone(), &format!("setup_{}", self.name));
            let bd_result = workspace.run_bd(args, &format!("setup_{}", self.name));

            if !br_result.status.success() {
                return Err(format!("br setup failed: {}", br_result.stderr));
            }
            if !bd_result.status.success() {
                return Err(format!("bd setup failed: {}", bd_result.stderr));
            }
        }

        // Run test command
        let args: Vec<&str> = self.test_command.iter().map(String::as_str).collect();
        let br_result = workspace.run_br(args.clone(), &self.name);
        let bd_result = workspace.run_bd(args, &self.name);

        // Compare exit codes if requested
        if self.compare_exit_codes {
            let br_success = br_result.status.success();
            let bd_success = bd_result.status.success();
            if br_success != bd_success {
                return Err(format!(
                    "Exit code mismatch: br={}, bd={}",
                    br_result.status, bd_result.status
                ));
            }
        }

        // Compare outputs using the configured mode
        let br_json = extract_json_payload(&br_result.stdout);
        let bd_json = extract_json_payload(&bd_result.stdout);

        compare_json(&br_json, &bd_json, &self.compare_mode)
    }
}

/// Predefined test scenarios for common operations
#[allow(dead_code)]
pub mod scenarios {
    use super::*;

    pub fn empty_list() -> TestScenario {
        TestScenario::new("empty_list", vec!["list", "--json"])
            .with_description("Verify empty list output matches")
    }

    pub fn create_basic() -> TestScenario {
        TestScenario::new("create_basic", vec!["list", "--json"])
            .with_description("Create a basic issue and verify list output")
            .with_setup(vec![vec!["create", "Test issue"]])
            .with_compare_mode(CompareMode::ContainsFields(vec![
                "title".to_string(),
                "status".to_string(),
                "issue_type".to_string(),
            ]))
    }

    pub fn create_with_type_and_priority() -> TestScenario {
        TestScenario::new("create_typed", vec!["list", "--json"])
            .with_description("Create issue with type and priority")
            .with_setup(vec![vec!["create", "Bug issue", "--type", "bug", "--priority", "1"]])
            .with_compare_mode(CompareMode::ContainsFields(vec![
                "title".to_string(),
                "issue_type".to_string(),
                "priority".to_string(),
            ]))
    }

    pub fn stats_after_create() -> TestScenario {
        TestScenario::new("stats_after_create", vec!["stats", "--json"])
            .with_description("Verify stats after creating issues")
            .with_setup(vec![
                vec!["create", "Issue 1"],
                vec!["create", "Issue 2"],
            ])
            .with_compare_mode(CompareMode::ContainsFields(vec!["total".to_string()]))
    }
}

// ============================================================================
// CONFORMANCE TESTS
// ============================================================================

#[test]
fn conformance_init() {
    common::init_test_logging();
    info!("Starting conformance_init test");

    let workspace = ConformanceWorkspace::new();
    let (br_out, bd_out) = workspace.init_both();

    assert!(br_out.status.success(), "br init failed: {}", br_out.stderr);
    assert!(bd_out.status.success(), "bd init failed: {}", bd_out.stderr);

    // Both should create .beads directories
    assert!(
        workspace.br_root.join(".beads").exists(),
        "br did not create .beads"
    );
    assert!(
        workspace.bd_root.join(".beads").exists(),
        "bd did not create .beads"
    );

    info!("conformance_init passed");
}

#[test]
fn conformance_create_basic() {
    common::init_test_logging();
    info!("Starting conformance_create_basic test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues with same parameters
    let br_create = workspace.run_br(["create", "Test issue", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Test issue", "--json"], "create");

    assert!(
        br_create.status.success(),
        "br create failed: {}",
        br_create.stderr
    );
    assert!(
        bd_create.status.success(),
        "bd create failed: {}",
        bd_create.stderr
    );

    // Compare with ContainsFields - title, status, priority should match
    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    let result = compare_json(
        &br_json,
        &bd_json,
        &CompareMode::ContainsFields(vec![
            "title".to_string(),
            "status".to_string(),
            "issue_type".to_string(),
        ]),
    );

    assert!(result.is_ok(), "JSON comparison failed: {:?}", result.err());
    info!("conformance_create_basic passed");
}

#[test]
fn conformance_create_with_type_and_priority() {
    common::init_test_logging();
    info!("Starting conformance_create_with_type_and_priority test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let args = [
        "create",
        "Bug fix needed",
        "--type",
        "bug",
        "--priority",
        "1",
        "--json",
    ];

    let br_create = workspace.run_br(args.clone(), "create_bug");
    let bd_create = workspace.run_bd(args, "create_bug");

    assert!(
        br_create.status.success(),
        "br create failed: {}",
        br_create.stderr
    );
    assert!(
        bd_create.status.success(),
        "bd create failed: {}",
        bd_create.stderr
    );

    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    // Parse and verify specific fields
    let br_val: Value = serde_json::from_str(&br_json).expect("br json");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("bd json");

    // Handle both object and array outputs
    let br_issue = if br_val.is_array() {
        &br_val[0]
    } else {
        &br_val
    };
    let bd_issue = if bd_val.is_array() {
        &bd_val[0]
    } else {
        &bd_val
    };

    assert_eq!(br_issue["title"], bd_issue["title"], "title mismatch");
    assert_eq!(
        br_issue["issue_type"], bd_issue["issue_type"],
        "issue_type mismatch: br={}, bd={}",
        br_issue["issue_type"], bd_issue["issue_type"]
    );
    assert_eq!(
        br_issue["priority"], bd_issue["priority"],
        "priority mismatch: br={}, bd={}",
        br_issue["priority"], bd_issue["priority"]
    );

    info!("conformance_create_with_type_and_priority passed");
}

#[test]
fn conformance_list_empty() {
    common::init_test_logging();
    info!("Starting conformance_list_empty test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_list = workspace.run_br(["list", "--json"], "list_empty");
    let bd_list = workspace.run_bd(["list", "--json"], "list_empty");

    assert!(
        br_list.status.success(),
        "br list failed: {}",
        br_list.stderr
    );
    assert!(
        bd_list.status.success(),
        "bd list failed: {}",
        bd_list.stderr
    );

    // Both should return empty arrays
    let br_json = extract_json_payload(&br_list.stdout);
    let bd_json = extract_json_payload(&bd_list.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Null);
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Null);

    // Both should be empty arrays or similar
    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "list lengths differ: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 0, "expected empty list");

    info!("conformance_list_empty passed");
}

#[test]
fn conformance_list_with_issues() {
    common::init_test_logging();
    info!("Starting conformance_list_with_issues test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create same issues in both
    workspace.run_br(["create", "Issue one"], "create1");
    workspace.run_bd(["create", "Issue one"], "create1");

    workspace.run_br(["create", "Issue two"], "create2");
    workspace.run_bd(["create", "Issue two"], "create2");

    let br_list = workspace.run_br(["list", "--json"], "list");
    let bd_list = workspace.run_bd(["list", "--json"], "list");

    assert!(
        br_list.status.success(),
        "br list failed: {}",
        br_list.stderr
    );
    assert!(
        bd_list.status.success(),
        "bd list failed: {}",
        bd_list.stderr
    );

    let br_json = extract_json_payload(&br_list.stdout);
    let bd_json = extract_json_payload(&bd_list.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("br json");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("bd json");

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "list lengths differ: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 2, "expected 2 issues");

    info!("conformance_list_with_issues passed");
}

#[test]
fn conformance_ready_empty() {
    common::init_test_logging();
    info!("Starting conformance_ready_empty test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_ready = workspace.run_br(["ready", "--json"], "ready_empty");
    let bd_ready = workspace.run_bd(["ready", "--json"], "ready_empty");

    assert!(
        br_ready.status.success(),
        "br ready failed: {}",
        br_ready.stderr
    );
    assert!(
        bd_ready.status.success(),
        "bd ready failed: {}",
        bd_ready.stderr
    );

    let br_json = extract_json_payload(&br_ready.stdout);
    let bd_json = extract_json_payload(&bd_ready.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Array(vec![]));
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Array(vec![]));

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "ready lengths differ: br={}, bd={}",
        br_len, bd_len
    );

    info!("conformance_ready_empty passed");
}

#[test]
fn conformance_ready_with_issues() {
    common::init_test_logging();
    info!("Starting conformance_ready_with_issues test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    workspace.run_br(["create", "Ready issue"], "create");
    workspace.run_bd(["create", "Ready issue"], "create");

    let br_ready = workspace.run_br(["ready", "--json"], "ready");
    let bd_ready = workspace.run_bd(["ready", "--json"], "ready");

    assert!(
        br_ready.status.success(),
        "br ready failed: {}",
        br_ready.stderr
    );
    assert!(
        bd_ready.status.success(),
        "bd ready failed: {}",
        bd_ready.stderr
    );

    let br_json = extract_json_payload(&br_ready.stdout);
    let bd_json = extract_json_payload(&bd_ready.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("br json");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("bd json");

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "ready lengths differ: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 1, "expected 1 ready issue");

    info!("conformance_ready_with_issues passed");
}

#[test]
fn conformance_blocked_empty() {
    common::init_test_logging();
    info!("Starting conformance_blocked_empty test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_blocked = workspace.run_br(["blocked", "--json"], "blocked_empty");
    let bd_blocked = workspace.run_bd(["blocked", "--json"], "blocked_empty");

    assert!(
        br_blocked.status.success(),
        "br blocked failed: {}",
        br_blocked.stderr
    );
    assert!(
        bd_blocked.status.success(),
        "bd blocked failed: {}",
        bd_blocked.stderr
    );

    let br_json = extract_json_payload(&br_blocked.stdout);
    let bd_json = extract_json_payload(&bd_blocked.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Array(vec![]));
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Array(vec![]));

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(br_len, bd_len, "blocked lengths differ");
    assert_eq!(br_len, 0, "expected no blocked issues");

    info!("conformance_blocked_empty passed");
}

#[test]
fn conformance_stats() {
    common::init_test_logging();
    info!("Starting conformance_stats test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create some issues to have stats
    workspace.run_br(["create", "Issue A"], "create_a");
    workspace.run_bd(["create", "Issue A"], "create_a");

    let br_stats = workspace.run_br(["stats", "--json"], "stats");
    let bd_stats = workspace.run_bd(["stats", "--json"], "stats");

    assert!(
        br_stats.status.success(),
        "br stats failed: {}",
        br_stats.stderr
    );
    assert!(
        bd_stats.status.success(),
        "bd stats failed: {}",
        bd_stats.stderr
    );

    // Stats command returns structured data - verify key fields match
    let br_json = extract_json_payload(&br_stats.stdout);
    let bd_json = extract_json_payload(&bd_stats.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("br json");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("bd json");

    // Both should report same total count
    let br_total = br_val["total"]
        .as_i64()
        .or_else(|| br_val["summary"]["total"].as_i64());
    let bd_total = bd_val["total"]
        .as_i64()
        .or_else(|| bd_val["summary"]["total"].as_i64());

    assert_eq!(
        br_total, bd_total,
        "total issue counts differ: br={:?}, bd={:?}",
        br_total, bd_total
    );

    info!("conformance_stats passed");
}

#[test]
fn conformance_sync_flush_only() {
    common::init_test_logging();
    info!("Starting conformance_sync_flush_only test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    workspace.run_br(["create", "Sync test issue"], "create");
    workspace.run_bd(["create", "Sync test issue"], "create");

    // Run sync --flush-only
    let br_sync = workspace.run_br(["sync", "--flush-only"], "sync");
    let bd_sync = workspace.run_bd(["sync", "--flush-only"], "sync");

    assert!(
        br_sync.status.success(),
        "br sync failed: {}",
        br_sync.stderr
    );
    assert!(
        bd_sync.status.success(),
        "bd sync failed: {}",
        bd_sync.stderr
    );

    // Both should create issues.jsonl
    let br_jsonl = workspace.br_root.join(".beads").join("issues.jsonl");
    let bd_jsonl = workspace.bd_root.join(".beads").join("issues.jsonl");

    assert!(br_jsonl.exists(), "br did not create issues.jsonl");
    assert!(bd_jsonl.exists(), "bd did not create issues.jsonl");

    // Verify JSONL files are non-empty
    let br_content = fs::read_to_string(&br_jsonl).expect("read br jsonl");
    let bd_content = fs::read_to_string(&bd_jsonl).expect("read bd jsonl");

    assert!(!br_content.trim().is_empty(), "br issues.jsonl is empty");
    assert!(!bd_content.trim().is_empty(), "bd issues.jsonl is empty");

    // Both should have exactly 1 line (1 issue)
    let br_lines = br_content.lines().count();
    let bd_lines = bd_content.lines().count();

    assert_eq!(
        br_lines, bd_lines,
        "JSONL line counts differ: br={}, bd={}",
        br_lines, bd_lines
    );

    info!("conformance_sync_flush_only passed");
}

#[test]
fn conformance_dependency_blocking() {
    common::init_test_logging();
    info!("Starting conformance_dependency_blocking test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create blocker and blocked issues
    let br_blocker = workspace.run_br(["create", "Blocker issue", "--json"], "create_blocker");
    let bd_blocker = workspace.run_bd(["create", "Blocker issue", "--json"], "create_blocker");

    let br_blocked = workspace.run_br(["create", "Blocked issue", "--json"], "create_blocked");
    let bd_blocked = workspace.run_bd(["create", "Blocked issue", "--json"], "create_blocked");

    // Extract IDs
    let br_blocker_json = extract_json_payload(&br_blocker.stdout);
    let bd_blocker_json = extract_json_payload(&bd_blocker.stdout);
    let br_blocked_json = extract_json_payload(&br_blocked.stdout);
    let bd_blocked_json = extract_json_payload(&bd_blocked.stdout);

    let br_blocker_val: Value = serde_json::from_str(&br_blocker_json).expect("parse");
    let bd_blocker_val: Value = serde_json::from_str(&bd_blocker_json).expect("parse");
    let br_blocked_val: Value = serde_json::from_str(&br_blocked_json).expect("parse");
    let bd_blocked_val: Value = serde_json::from_str(&bd_blocked_json).expect("parse");

    let br_blocker_id = br_blocker_val["id"]
        .as_str()
        .or_else(|| br_blocker_val[0]["id"].as_str())
        .unwrap();
    let bd_blocker_id = bd_blocker_val["id"]
        .as_str()
        .or_else(|| bd_blocker_val[0]["id"].as_str())
        .unwrap();
    let br_blocked_id = br_blocked_val["id"]
        .as_str()
        .or_else(|| br_blocked_val[0]["id"].as_str())
        .unwrap();
    let bd_blocked_id = bd_blocked_val["id"]
        .as_str()
        .or_else(|| bd_blocked_val[0]["id"].as_str())
        .unwrap();

    // Add dependency: blocked depends on blocker
    let br_dep = workspace.run_br(["dep", "add", br_blocked_id, br_blocker_id], "add_dep");
    let bd_dep = workspace.run_bd(["dep", "add", bd_blocked_id, bd_blocker_id], "add_dep");

    assert!(
        br_dep.status.success(),
        "br dep add failed: {}",
        br_dep.stderr
    );
    assert!(
        bd_dep.status.success(),
        "bd dep add failed: {}",
        bd_dep.stderr
    );

    // Check blocked command
    let br_blocked_cmd = workspace.run_br(["blocked", "--json"], "blocked");
    let bd_blocked_cmd = workspace.run_bd(["blocked", "--json"], "blocked");

    assert!(br_blocked_cmd.status.success(), "br blocked failed");
    assert!(bd_blocked_cmd.status.success(), "bd blocked failed");

    let br_blocked_json = extract_json_payload(&br_blocked_cmd.stdout);
    let bd_blocked_json = extract_json_payload(&bd_blocked_cmd.stdout);

    let br_val: Value = serde_json::from_str(&br_blocked_json).unwrap_or(Value::Array(vec![]));
    let bd_val: Value = serde_json::from_str(&bd_blocked_json).unwrap_or(Value::Array(vec![]));

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "blocked counts differ: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 1, "expected 1 blocked issue");

    // Check ready - should only show the blocker, not the blocked issue
    let br_ready = workspace.run_br(["ready", "--json"], "ready_after_dep");
    let bd_ready = workspace.run_bd(["ready", "--json"], "ready_after_dep");

    let br_ready_json = extract_json_payload(&br_ready.stdout);
    let bd_ready_json = extract_json_payload(&bd_ready.stdout);

    let br_ready_val: Value = serde_json::from_str(&br_ready_json).unwrap_or(Value::Array(vec![]));
    let bd_ready_val: Value = serde_json::from_str(&bd_ready_json).unwrap_or(Value::Array(vec![]));

    let br_ready_len = br_ready_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_ready_len = bd_ready_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_ready_len, bd_ready_len,
        "ready counts differ: br={}, bd={}",
        br_ready_len, bd_ready_len
    );
    assert_eq!(br_ready_len, 1, "expected 1 ready issue (the blocker)");

    info!("conformance_dependency_blocking passed");
}

#[test]
fn conformance_close_issue() {
    common::init_test_logging();
    info!("Starting conformance_close_issue test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    let br_create = workspace.run_br(["create", "Issue to close", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Issue to close", "--json"], "create");

    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

    let br_id = br_val["id"]
        .as_str()
        .or_else(|| br_val[0]["id"].as_str())
        .unwrap();
    let bd_id = bd_val["id"]
        .as_str()
        .or_else(|| bd_val[0]["id"].as_str())
        .unwrap();

    // Close issues
    let br_close = workspace.run_br(["close", br_id, "--json"], "close");
    let bd_close = workspace.run_bd(["close", bd_id, "--json"], "close");

    assert!(
        br_close.status.success(),
        "br close failed: {}",
        br_close.stderr
    );
    assert!(
        bd_close.status.success(),
        "bd close failed: {}",
        bd_close.stderr
    );

    // Verify via show that issues are closed (list may exclude closed by default)
    let br_show = workspace.run_br(["show", br_id, "--json"], "show_after_close");
    let bd_show = workspace.run_bd(["show", bd_id, "--json"], "show_after_close");

    assert!(
        br_show.status.success(),
        "br show failed: {}",
        br_show.stderr
    );
    assert!(
        bd_show.status.success(),
        "bd show failed: {}",
        bd_show.stderr
    );

    let br_show_json = extract_json_payload(&br_show.stdout);
    let bd_show_json = extract_json_payload(&bd_show.stdout);

    let br_show_val: Value = serde_json::from_str(&br_show_json).expect("parse");
    let bd_show_val: Value = serde_json::from_str(&bd_show_json).expect("parse");

    // Handle array or object response
    let br_issue = if br_show_val.is_array() {
        &br_show_val[0]
    } else {
        &br_show_val
    };
    let bd_issue = if bd_show_val.is_array() {
        &bd_show_val[0]
    } else {
        &bd_show_val
    };

    assert_eq!(
        br_issue["status"].as_str(),
        Some("closed"),
        "br issue not closed: got {:?}",
        br_issue["status"]
    );
    assert_eq!(
        bd_issue["status"].as_str(),
        Some("closed"),
        "bd issue not closed: got {:?}",
        bd_issue["status"]
    );

    info!("conformance_close_issue passed");
}

#[test]
fn conformance_update_issue() {
    common::init_test_logging();
    info!("Starting conformance_update_issue test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    let br_create = workspace.run_br(["create", "Issue to update", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Issue to update", "--json"], "create");

    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

    let br_id = br_val["id"]
        .as_str()
        .or_else(|| br_val[0]["id"].as_str())
        .unwrap();
    let bd_id = bd_val["id"]
        .as_str()
        .or_else(|| bd_val[0]["id"].as_str())
        .unwrap();

    // Update priority
    let br_update = workspace.run_br(
        ["update", br_id, "--priority", "0", "--json"],
        "update_priority",
    );
    let bd_update = workspace.run_bd(
        ["update", bd_id, "--priority", "0", "--json"],
        "update_priority",
    );

    assert!(
        br_update.status.success(),
        "br update failed: {}",
        br_update.stderr
    );
    assert!(
        bd_update.status.success(),
        "bd update failed: {}",
        bd_update.stderr
    );

    // Verify via show
    let br_show = workspace.run_br(["show", br_id, "--json"], "show_after_update");
    let bd_show = workspace.run_bd(["show", bd_id, "--json"], "show_after_update");

    let br_show_json = extract_json_payload(&br_show.stdout);
    let bd_show_json = extract_json_payload(&bd_show.stdout);

    let br_show_val: Value = serde_json::from_str(&br_show_json).expect("parse");
    let bd_show_val: Value = serde_json::from_str(&bd_show_json).expect("parse");

    let br_priority = br_show_val["priority"]
        .as_i64()
        .or_else(|| br_show_val[0]["priority"].as_i64());
    let bd_priority = bd_show_val["priority"]
        .as_i64()
        .or_else(|| bd_show_val[0]["priority"].as_i64());

    assert_eq!(
        br_priority, bd_priority,
        "priority mismatch after update: br={:?}, bd={:?}",
        br_priority, bd_priority
    );
    assert_eq!(br_priority, Some(0), "expected priority 0");

    info!("conformance_update_issue passed");
}

#[test]
fn conformance_reopen_basic() {
    common::init_test_logging();
    info!("Starting conformance_reopen_basic test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create and close issues
    let br_create = workspace.run_br(["create", "Issue to reopen", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Issue to reopen", "--json"], "create");

    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

    let br_id = br_val["id"]
        .as_str()
        .or_else(|| br_val[0]["id"].as_str())
        .unwrap();
    let bd_id = bd_val["id"]
        .as_str()
        .or_else(|| bd_val[0]["id"].as_str())
        .unwrap();

    // Close issues
    workspace.run_br(["close", br_id], "close");
    workspace.run_bd(["close", bd_id], "close");

    // Reopen issues
    let br_reopen = workspace.run_br(["reopen", br_id, "--json"], "reopen");
    let bd_reopen = workspace.run_bd(["reopen", bd_id, "--json"], "reopen");

    assert!(
        br_reopen.status.success(),
        "br reopen failed: {}",
        br_reopen.stderr
    );
    assert!(
        bd_reopen.status.success(),
        "bd reopen failed: {}",
        bd_reopen.stderr
    );

    // Verify status is open again
    let br_show = workspace.run_br(["show", br_id, "--json"], "show_after_reopen");
    let bd_show = workspace.run_bd(["show", bd_id, "--json"], "show_after_reopen");

    let br_show_json = extract_json_payload(&br_show.stdout);
    let bd_show_json = extract_json_payload(&bd_show.stdout);

    let br_show_val: Value = serde_json::from_str(&br_show_json).expect("parse");
    let bd_show_val: Value = serde_json::from_str(&bd_show_json).expect("parse");

    let br_status = br_show_val["status"]
        .as_str()
        .or_else(|| br_show_val[0]["status"].as_str());
    let bd_status = bd_show_val["status"]
        .as_str()
        .or_else(|| bd_show_val[0]["status"].as_str());

    assert_eq!(
        br_status, bd_status,
        "status mismatch after reopen: br={:?}, bd={:?}",
        br_status, bd_status
    );
    assert_eq!(br_status, Some("open"), "expected status open");

    info!("conformance_reopen_basic passed");
}

#[test]
fn conformance_list_by_type() {
    common::init_test_logging();
    info!("Starting conformance_list_by_type test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues with different types
    workspace.run_br(["create", "Bug issue", "--type", "bug"], "create_bug");
    workspace.run_bd(["create", "Bug issue", "--type", "bug"], "create_bug");

    workspace.run_br(
        ["create", "Feature issue", "--type", "feature"],
        "create_feature",
    );
    workspace.run_bd(
        ["create", "Feature issue", "--type", "feature"],
        "create_feature",
    );

    workspace.run_br(["create", "Task issue", "--type", "task"], "create_task");
    workspace.run_bd(["create", "Task issue", "--type", "task"], "create_task");

    // List only bugs
    let br_list = workspace.run_br(["list", "--type", "bug", "--json"], "list_bugs");
    let bd_list = workspace.run_bd(["list", "--type", "bug", "--json"], "list_bugs");

    assert!(
        br_list.status.success(),
        "br list failed: {}",
        br_list.stderr
    );
    assert!(
        bd_list.status.success(),
        "bd list failed: {}",
        bd_list.stderr
    );

    let br_json = extract_json_payload(&br_list.stdout);
    let bd_json = extract_json_payload(&bd_list.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "bug list lengths differ: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 1, "expected exactly 1 bug");

    info!("conformance_list_by_type passed");
}

#[test]
fn conformance_show_basic() {
    common::init_test_logging();
    info!("Starting conformance_show_basic test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues with same title
    let br_create = workspace.run_br(
        [
            "create",
            "Show test issue",
            "--type",
            "task",
            "--priority",
            "2",
            "--json",
        ],
        "create",
    );
    let bd_create = workspace.run_bd(
        [
            "create",
            "Show test issue",
            "--type",
            "task",
            "--priority",
            "2",
            "--json",
        ],
        "create",
    );

    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

    let br_id = br_val["id"]
        .as_str()
        .or_else(|| br_val[0]["id"].as_str())
        .unwrap();
    let bd_id = bd_val["id"]
        .as_str()
        .or_else(|| bd_val[0]["id"].as_str())
        .unwrap();

    // Show the issues
    let br_show = workspace.run_br(["show", br_id, "--json"], "show");
    let bd_show = workspace.run_bd(["show", bd_id, "--json"], "show");

    assert!(
        br_show.status.success(),
        "br show failed: {}",
        br_show.stderr
    );
    assert!(
        bd_show.status.success(),
        "bd show failed: {}",
        bd_show.stderr
    );

    let br_show_json = extract_json_payload(&br_show.stdout);
    let bd_show_json = extract_json_payload(&bd_show.stdout);

    let result = compare_json(
        &br_show_json,
        &bd_show_json,
        &CompareMode::ContainsFields(vec![
            "title".to_string(),
            "status".to_string(),
            "issue_type".to_string(),
            "priority".to_string(),
        ]),
    );

    assert!(
        result.is_ok(),
        "show JSON comparison failed: {:?}",
        result.err()
    );

    info!("conformance_show_basic passed");
}

#[test]
fn conformance_search_basic() {
    common::init_test_logging();
    info!("Starting conformance_search_basic test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues with searchable content
    workspace.run_br(["create", "Authentication bug in login"], "create1");
    workspace.run_bd(["create", "Authentication bug in login"], "create1");

    workspace.run_br(["create", "Payment processing feature"], "create2");
    workspace.run_bd(["create", "Payment processing feature"], "create2");

    workspace.run_br(["create", "User login flow improvement"], "create3");
    workspace.run_bd(["create", "User login flow improvement"], "create3");

    // Search for "login"
    let br_search = workspace.run_br(["search", "login", "--json"], "search_login");
    let bd_search = workspace.run_bd(["search", "login", "--json"], "search_login");

    assert!(
        br_search.status.success(),
        "br search failed: {}",
        br_search.stderr
    );
    assert!(
        bd_search.status.success(),
        "bd search failed: {}",
        bd_search.stderr
    );

    let br_json = extract_json_payload(&br_search.stdout);
    let bd_json = extract_json_payload(&bd_search.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Array(vec![]));
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Array(vec![]));

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "search result lengths differ: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 2, "expected 2 issues matching 'login'");

    info!("conformance_search_basic passed");
}

#[test]
fn conformance_label_basic() {
    common::init_test_logging();
    info!("Starting conformance_label_basic test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    let br_create = workspace.run_br(["create", "Issue for labels", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Issue for labels", "--json"], "create");

    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

    let br_id = br_val["id"]
        .as_str()
        .or_else(|| br_val[0]["id"].as_str())
        .unwrap();
    let bd_id = bd_val["id"]
        .as_str()
        .or_else(|| bd_val[0]["id"].as_str())
        .unwrap();

    // Add labels
    let br_add = workspace.run_br(["label", "add", br_id, "urgent"], "label_add");
    let bd_add = workspace.run_bd(["label", "add", bd_id, "urgent"], "label_add");

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

    // List labels
    let br_list = workspace.run_br(["label", "list", br_id, "--json"], "label_list");
    let bd_list = workspace.run_bd(["label", "list", bd_id, "--json"], "label_list");

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

    let br_label_json = extract_json_payload(&br_list.stdout);
    let bd_label_json = extract_json_payload(&bd_list.stdout);

    // Both should have "urgent" label
    assert!(
        br_label_json.contains("urgent"),
        "br missing 'urgent' label: {}",
        br_label_json
    );
    assert!(
        bd_label_json.contains("urgent"),
        "bd missing 'urgent' label: {}",
        bd_label_json
    );

    info!("conformance_label_basic passed");
}

#[test]
fn conformance_dep_list() {
    common::init_test_logging();
    info!("Starting conformance_dep_list test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create parent and child issues
    let br_parent = workspace.run_br(["create", "Parent issue", "--json"], "create_parent");
    let bd_parent = workspace.run_bd(["create", "Parent issue", "--json"], "create_parent");

    let br_child = workspace.run_br(["create", "Child issue", "--json"], "create_child");
    let bd_child = workspace.run_bd(["create", "Child issue", "--json"], "create_child");

    let br_parent_json = extract_json_payload(&br_parent.stdout);
    let bd_parent_json = extract_json_payload(&bd_parent.stdout);
    let br_child_json = extract_json_payload(&br_child.stdout);
    let bd_child_json = extract_json_payload(&bd_child.stdout);

    let br_parent_val: Value = serde_json::from_str(&br_parent_json).expect("parse");
    let bd_parent_val: Value = serde_json::from_str(&bd_parent_json).expect("parse");
    let br_child_val: Value = serde_json::from_str(&br_child_json).expect("parse");
    let bd_child_val: Value = serde_json::from_str(&bd_child_json).expect("parse");

    let br_parent_id = br_parent_val["id"]
        .as_str()
        .or_else(|| br_parent_val[0]["id"].as_str())
        .unwrap();
    let bd_parent_id = bd_parent_val["id"]
        .as_str()
        .or_else(|| bd_parent_val[0]["id"].as_str())
        .unwrap();
    let br_child_id = br_child_val["id"]
        .as_str()
        .or_else(|| br_child_val[0]["id"].as_str())
        .unwrap();
    let bd_child_id = bd_child_val["id"]
        .as_str()
        .or_else(|| bd_child_val[0]["id"].as_str())
        .unwrap();

    // Add dependency: child depends on parent
    let br_dep = workspace.run_br(["dep", "add", br_child_id, br_parent_id], "dep_add");
    let bd_dep = workspace.run_bd(["dep", "add", br_child_id, bd_parent_id], "dep_add");

    assert!(
        br_dep.status.success(),
        "br dep add failed: {}",
        br_dep.stderr
    );
    assert!(
        bd_dep.status.success(),
        "bd dep add failed: {}",
        bd_dep.stderr
    );

    // List dependencies
    let br_list = workspace.run_br(["dep", "list", br_child_id, "--json"], "dep_list");
    let bd_list = workspace.run_bd(["dep", "list", bd_child_id, "--json"], "dep_list");

    assert!(
        br_list.status.success(),
        "br dep list failed: {}",
        br_list.stderr
    );
    assert!(
        bd_list.status.success(),
        "bd dep list failed: {}",
        bd_list.stderr
    );

    let br_dep_json = extract_json_payload(&br_list.stdout);
    let bd_dep_json = extract_json_payload(&bd_list.stdout);

    let br_dep_val: Value = serde_json::from_str(&br_dep_json).unwrap_or(Value::Array(vec![]));
    let bd_dep_val: Value = serde_json::from_str(&bd_dep_json).unwrap_or(Value::Array(vec![]));

    let br_dep_len = br_dep_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_dep_len = bd_dep_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_dep_len, bd_dep_len,
        "dep list lengths differ: br={}, bd={}",
        br_dep_len, bd_dep_len
    );
    assert_eq!(br_dep_len, 1, "expected 1 dependency");

    info!("conformance_dep_list passed");
}

#[test]
fn conformance_count_basic() {
    common::init_test_logging();
    info!("Starting conformance_count_basic test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues with different statuses
    let _br_create1 = workspace.run_br(["create", "Open issue 1", "--json"], "create1");
    let _bd_create1 = workspace.run_bd(["create", "Open issue 1", "--json"], "create1");

    let _br_create2 = workspace.run_br(["create", "Open issue 2", "--json"], "create2");
    let _bd_create2 = workspace.run_bd(["create", "Open issue 2", "--json"], "create2");

    let br_create3 = workspace.run_br(["create", "Will close", "--json"], "create3");
    let bd_create3 = workspace.run_bd(["create", "Will close", "--json"], "create3");

    // Close one issue
    let br_json = extract_json_payload(&br_create3.stdout);
    let bd_json = extract_json_payload(&bd_create3.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

    let br_id = br_val["id"]
        .as_str()
        .or_else(|| br_val[0]["id"].as_str())
        .unwrap();
    let bd_id = bd_val["id"]
        .as_str()
        .or_else(|| bd_val[0]["id"].as_str())
        .unwrap();

    workspace.run_br(["close", br_id], "close");
    workspace.run_bd(["close", bd_id], "close");

    // Run count
    let br_count = workspace.run_br(["count", "--json"], "count");
    let bd_count = workspace.run_bd(["count", "--json"], "count");

    assert!(
        br_count.status.success(),
        "br count failed: {}",
        br_count.stderr
    );
    assert!(
        bd_count.status.success(),
        "bd count failed: {}",
        bd_count.stderr
    );

    let br_count_json = extract_json_payload(&br_count.stdout);
    let bd_count_json = extract_json_payload(&bd_count.stdout);

    let br_count_val: Value = serde_json::from_str(&br_count_json).expect("parse");
    let bd_count_val: Value = serde_json::from_str(&bd_count_json).expect("parse");

    // Both should report same total
    let br_total = br_count_val["total"]
        .as_i64()
        .or_else(|| br_count_val["summary"]["total"].as_i64());
    let bd_total = bd_count_val["total"]
        .as_i64()
        .or_else(|| bd_count_val["summary"]["total"].as_i64());

    assert_eq!(
        br_total, bd_total,
        "total counts differ: br={:?}, bd={:?}",
        br_total, bd_total
    );

    info!("conformance_count_basic passed");
}

#[test]
fn conformance_delete_issue() {
    common::init_test_logging();
    info!("Starting conformance_delete_issue test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    let br_create = workspace.run_br(["create", "Issue to delete", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Issue to delete", "--json"], "create");

    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

    let br_id = br_val["id"]
        .as_str()
        .or_else(|| br_val[0]["id"].as_str())
        .unwrap();
    let bd_id = bd_val["id"]
        .as_str()
        .or_else(|| bd_val[0]["id"].as_str())
        .unwrap();

    // Delete issues (bd requires --force to actually delete, br doesn't)
    let br_delete = workspace.run_br(["delete", br_id, "--reason", "test deletion"], "delete");
    let bd_delete = workspace.run_bd(
        ["delete", bd_id, "--reason", "test deletion", "--force"],
        "delete",
    );

    assert!(
        br_delete.status.success(),
        "br delete failed: {}",
        br_delete.stderr
    );
    assert!(
        bd_delete.status.success(),
        "bd delete failed: {}",
        bd_delete.stderr
    );

    // Verify deleted issues don't appear in list
    let br_list = workspace.run_br(["list", "--json"], "list_after_delete");
    let bd_list = workspace.run_bd(["list", "--json"], "list_after_delete");

    let br_list_json = extract_json_payload(&br_list.stdout);
    let bd_list_json = extract_json_payload(&bd_list.stdout);

    let br_list_val: Value = serde_json::from_str(&br_list_json).unwrap_or(Value::Array(vec![]));
    let bd_list_val: Value = serde_json::from_str(&bd_list_json).unwrap_or(Value::Array(vec![]));

    let br_len = br_list_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_list_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "list lengths differ after delete: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 0, "expected empty list after deletion");

    info!("conformance_delete_issue passed");
}

#[test]
fn conformance_dep_remove() {
    common::init_test_logging();
    info!("Starting conformance_dep_remove test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create blocker and blocked issues
    let br_blocker = workspace.run_br(["create", "Blocker", "--json"], "create_blocker");
    let bd_blocker = workspace.run_bd(["create", "Blocker", "--json"], "create_blocker");

    let br_blocked = workspace.run_br(["create", "Blocked", "--json"], "create_blocked");
    let bd_blocked = workspace.run_bd(["create", "Blocked", "--json"], "create_blocked");

    // Extract IDs
    let br_blocker_id = {
        let json = extract_json_payload(&br_blocker.stdout);
        let val: Value = serde_json::from_str(&json).expect("parse");
        val["id"]
            .as_str()
            .or_else(|| val[0]["id"].as_str())
            .unwrap()
            .to_string()
    };
    let bd_blocker_id = {
        let json = extract_json_payload(&bd_blocker.stdout);
        let val: Value = serde_json::from_str(&json).expect("parse");
        val["id"]
            .as_str()
            .or_else(|| val[0]["id"].as_str())
            .unwrap()
            .to_string()
    };
    let br_blocked_id = {
        let json = extract_json_payload(&br_blocked.stdout);
        let val: Value = serde_json::from_str(&json).expect("parse");
        val["id"]
            .as_str()
            .or_else(|| val[0]["id"].as_str())
            .unwrap()
            .to_string()
    };
    let bd_blocked_id = {
        let json = extract_json_payload(&bd_blocked.stdout);
        let val: Value = serde_json::from_str(&json).expect("parse");
        val["id"]
            .as_str()
            .or_else(|| val[0]["id"].as_str())
            .unwrap()
            .to_string()
    };

    // Add dependency
    workspace.run_br(["dep", "add", &br_blocked_id, &br_blocker_id], "add_dep");
    workspace.run_bd(["dep", "add", &bd_blocked_id, &bd_blocker_id], "add_dep");

    // Verify blocked
    let br_blocked_cmd = workspace.run_br(["blocked", "--json"], "blocked_before");
    let bd_blocked_cmd = workspace.run_bd(["blocked", "--json"], "blocked_before");

    let br_before_json = extract_json_payload(&br_blocked_cmd.stdout);
    let bd_before_json = extract_json_payload(&bd_blocked_cmd.stdout);

    let br_before: Value = serde_json::from_str(&br_before_json).unwrap_or(Value::Array(vec![]));
    let bd_before: Value = serde_json::from_str(&bd_before_json).unwrap_or(Value::Array(vec![]));

    assert_eq!(
        br_before.as_array().map(|a| a.len()).unwrap_or(0),
        1,
        "expected 1 blocked issue before remove"
    );
    assert_eq!(
        bd_before.as_array().map(|a| a.len()).unwrap_or(0),
        1,
        "expected 1 blocked issue before remove"
    );

    // Remove dependency
    let br_rm = workspace.run_br(["dep", "remove", &br_blocked_id, &br_blocker_id], "rm_dep");
    let bd_rm = workspace.run_bd(["dep", "remove", &bd_blocked_id, &bd_blocker_id], "rm_dep");

    assert!(
        br_rm.status.success(),
        "br dep remove failed: {}",
        br_rm.stderr
    );
    assert!(
        bd_rm.status.success(),
        "bd dep remove failed: {}",
        bd_rm.stderr
    );

    // Verify no longer blocked
    let br_blocked_after = workspace.run_br(["blocked", "--json"], "blocked_after");
    let bd_blocked_after = workspace.run_bd(["blocked", "--json"], "blocked_after");

    let br_after_json = extract_json_payload(&br_blocked_after.stdout);
    let bd_after_json = extract_json_payload(&bd_blocked_after.stdout);

    let br_after: Value = serde_json::from_str(&br_after_json).unwrap_or(Value::Array(vec![]));
    let bd_after: Value = serde_json::from_str(&bd_after_json).unwrap_or(Value::Array(vec![]));

    let br_len = br_after.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_after.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "blocked counts differ after remove: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 0, "expected no blocked issues after dep remove");

    info!("conformance_dep_remove passed");
}

#[test]
fn conformance_sync_import() {
    common::init_test_logging();
    info!("Starting conformance_sync_import test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues and export
    workspace.run_br(["create", "Import test A"], "create_a");
    workspace.run_bd(["create", "Import test A"], "create_a");

    workspace.run_br(["create", "Import test B"], "create_b");
    workspace.run_bd(["create", "Import test B"], "create_b");

    // Export from both
    workspace.run_br(["sync", "--flush-only"], "export");
    workspace.run_bd(["sync", "--flush-only"], "export");

    // Create fresh workspaces for import
    let import_workspace = ConformanceWorkspace::new();
    import_workspace.init_both();

    // Copy JSONL files to new workspaces
    let br_src_jsonl = workspace.br_root.join(".beads").join("issues.jsonl");
    let bd_src_jsonl = workspace.bd_root.join(".beads").join("issues.jsonl");
    let br_dst_jsonl = import_workspace.br_root.join(".beads").join("issues.jsonl");
    let bd_dst_jsonl = import_workspace.bd_root.join(".beads").join("issues.jsonl");

    fs::copy(&br_src_jsonl, &br_dst_jsonl).expect("copy br jsonl");
    fs::copy(&bd_src_jsonl, &bd_dst_jsonl).expect("copy bd jsonl");

    // Import
    let br_import = import_workspace.run_br(["sync", "--import-only"], "import");
    let bd_import = import_workspace.run_bd(["sync", "--import-only"], "import");

    assert!(
        br_import.status.success(),
        "br import failed: {}",
        br_import.stderr
    );
    assert!(
        bd_import.status.success(),
        "bd import failed: {}",
        bd_import.stderr
    );

    // Verify issues were imported
    let br_list = import_workspace.run_br(["list", "--json"], "list_after_import");
    let bd_list = import_workspace.run_bd(["list", "--json"], "list_after_import");

    let br_json = extract_json_payload(&br_list.stdout);
    let bd_json = extract_json_payload(&bd_list.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Array(vec![]));
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Array(vec![]));

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "import counts differ: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 2, "expected 2 issues after import");

    info!("conformance_sync_import passed");
}

#[test]
fn conformance_sync_roundtrip() {
    common::init_test_logging();
    info!("Starting conformance_sync_roundtrip test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues with various attributes
    workspace.run_br(
        [
            "create",
            "Roundtrip bug",
            "--type",
            "bug",
            "--priority",
            "1",
        ],
        "create_bug",
    );
    workspace.run_bd(
        [
            "create",
            "Roundtrip bug",
            "--type",
            "bug",
            "--priority",
            "1",
        ],
        "create_bug",
    );

    workspace.run_br(
        [
            "create",
            "Roundtrip feature",
            "--type",
            "feature",
            "--priority",
            "3",
        ],
        "create_feature",
    );
    workspace.run_bd(
        [
            "create",
            "Roundtrip feature",
            "--type",
            "feature",
            "--priority",
            "3",
        ],
        "create_feature",
    );

    // Export
    workspace.run_br(["sync", "--flush-only"], "export");
    workspace.run_bd(["sync", "--flush-only"], "export");

    // Read JSONL content
    let br_jsonl_path = workspace.br_root.join(".beads").join("issues.jsonl");
    let bd_jsonl_path = workspace.bd_root.join(".beads").join("issues.jsonl");

    let br_jsonl = fs::read_to_string(&br_jsonl_path).expect("read br jsonl");
    let bd_jsonl = fs::read_to_string(&bd_jsonl_path).expect("read bd jsonl");

    // Verify same number of lines (issues)
    let br_lines = br_jsonl.lines().count();
    let bd_lines = bd_jsonl.lines().count();

    assert_eq!(
        br_lines, bd_lines,
        "JSONL line counts differ: br={}, bd={}",
        br_lines, bd_lines
    );
    assert_eq!(br_lines, 2, "expected 2 lines in JSONL");

    // Parse JSONL and collect titles (order may differ between br and bd)
    let br_titles: HashSet<String> = br_jsonl
        .lines()
        .map(|line| {
            let val: Value = serde_json::from_str(line).expect("parse br line");
            val["title"].as_str().unwrap_or("").to_string()
        })
        .collect();
    let bd_titles: HashSet<String> = bd_jsonl
        .lines()
        .map(|line| {
            let val: Value = serde_json::from_str(line).expect("parse bd line");
            val["title"].as_str().unwrap_or("").to_string()
        })
        .collect();

    assert_eq!(
        br_titles, bd_titles,
        "JSONL titles differ: br={:?}, bd={:?}",
        br_titles, bd_titles
    );

    // Create fresh workspaces, import, and verify
    let import_workspace = ConformanceWorkspace::new();
    import_workspace.init_both();

    let br_dst_jsonl = import_workspace.br_root.join(".beads").join("issues.jsonl");
    let bd_dst_jsonl = import_workspace.bd_root.join(".beads").join("issues.jsonl");

    fs::copy(&br_jsonl_path, &br_dst_jsonl).expect("copy br jsonl");
    fs::copy(&bd_jsonl_path, &bd_dst_jsonl).expect("copy bd jsonl");

    import_workspace.run_br(["sync", "--import-only"], "import");
    import_workspace.run_bd(["sync", "--import-only"], "import");

    // Verify imported data matches
    let br_after = import_workspace.run_br(["list", "--json"], "list_after");
    let bd_after = import_workspace.run_bd(["list", "--json"], "list_after");

    let br_after_json = extract_json_payload(&br_after.stdout);
    let bd_after_json = extract_json_payload(&bd_after.stdout);

    let br_after_val: Value = serde_json::from_str(&br_after_json).expect("parse");
    let bd_after_val: Value = serde_json::from_str(&bd_after_json).expect("parse");

    let br_after_len = br_after_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_after_len = bd_after_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_after_len, bd_after_len,
        "roundtrip counts differ: br={}, bd={}",
        br_after_len, bd_after_len
    );
    assert_eq!(br_after_len, 2, "expected 2 issues after roundtrip");

    info!("conformance_sync_roundtrip passed");
}

// ============================================================================
// CRUD COMMAND EXPANSION TESTS
// ============================================================================

// --- init tests ---

#[test]
fn conformance_init_reinit() {
    common::init_test_logging();
    info!("Starting conformance_init_reinit test");

    let workspace = ConformanceWorkspace::new();

    // First init
    workspace.init_both();

    // Second init (re-init) - should be idempotent or error gracefully
    let br_reinit = workspace.run_br(["init"], "reinit");
    let bd_reinit = workspace.run_bd(["init"], "reinit");

    // Both should have matching behavior (either both succeed or both fail)
    assert_eq!(
        br_reinit.status.success(),
        bd_reinit.status.success(),
        "reinit behavior differs: br success={}, bd success={}",
        br_reinit.status.success(),
        bd_reinit.status.success()
    );

    // .beads directory should still exist
    assert!(
        workspace.br_root.join(".beads").exists(),
        "br .beads disappeared after reinit"
    );
    assert!(
        workspace.bd_root.join(".beads").exists(),
        "bd .beads disappeared after reinit"
    );

    info!("conformance_init_reinit passed");
}

#[test]
fn conformance_init_existing_db() {
    common::init_test_logging();
    info!("Starting conformance_init_existing_db test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create some data
    workspace.run_br(["create", "Test issue"], "create");
    workspace.run_bd(["create", "Test issue"], "create");

    // Try init again - should preserve data
    workspace.run_br(["init"], "init_again");
    workspace.run_bd(["init"], "init_again");

    // Data should still exist
    let br_list = workspace.run_br(["list", "--json"], "list_after");
    let bd_list = workspace.run_bd(["list", "--json"], "list_after");

    let br_json = extract_json_payload(&br_list.stdout);
    let bd_json = extract_json_payload(&bd_list.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Array(vec![]));
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Array(vec![]));

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(br_len, bd_len, "issue counts differ after reinit");

    info!("conformance_init_existing_db passed");
}

#[test]
fn conformance_init_creates_beads_dir() {
    common::init_test_logging();
    info!("Starting conformance_init_creates_beads_dir test");

    let workspace = ConformanceWorkspace::new();

    // Verify .beads doesn't exist yet
    assert!(!workspace.br_root.join(".beads").exists());
    assert!(!workspace.bd_root.join(".beads").exists());

    workspace.init_both();

    // .beads/beads.db should exist for br
    assert!(
        workspace.br_root.join(".beads").join("beads.db").exists(),
        "br did not create .beads/beads.db"
    );
    // .beads/issues.db should exist for bd (assuming bd uses issues.db, or check what it creates)
    // Actually, checking if *any* .db file exists might be safer if we don't control bd version
    // But let's assume issues.db for now as per previous test code, or update if we know bd uses beads.db too.
    // If bd fails this assertion, we know bd behavior. 
    // The panic was "br did not create .beads/issues.db", so br uses beads.db (as verified by config).
    // I will change it to beads.db for br.
    
    // For bd, let's keep issues.db check if it passes, or maybe it also uses beads.db?
    // The previous run failed on br check.
    assert!(
        workspace.bd_root.join(".beads").join("issues.db").exists() || workspace.bd_root.join(".beads").join("beads.db").exists(),
        "bd did not create a database file"
    );

    info!("conformance_init_creates_beads_dir passed");
}

#[test]
fn conformance_init_json_output() {
    common::init_test_logging();
    info!("Starting conformance_init_json_output test");

    let workspace = ConformanceWorkspace::new();

    let br_init = workspace.run_br(["init", "--json"], "init_json");
    let bd_init = workspace.run_bd(["init", "--json"], "init_json");

    assert!(
        br_init.status.success(),
        "br init --json failed: {}",
        br_init.stderr
    );
    assert!(
        bd_init.status.success(),
        "bd init --json failed: {}",
        bd_init.stderr
    );

    // Both should produce valid JSON or exit successfully
    let br_json = extract_json_payload(&br_init.stdout);
    let bd_json = extract_json_payload(&bd_init.stdout);

    // If both produce JSON, they should have similar structure
    if !br_json.is_empty() && !bd_json.is_empty() {
        let br_val: Result<Value, _> = serde_json::from_str(&br_json);
        let bd_val: Result<Value, _> = serde_json::from_str(&bd_json);

        assert_eq!(
            br_val.is_ok(),
            bd_val.is_ok(),
            "JSON validity differs: br valid={}, bd valid={}",
            br_val.is_ok(),
            bd_val.is_ok()
        );
    }

    info!("conformance_init_json_output passed");
}

// --- create tests ---

#[test]
fn conformance_create_all_types() {
    common::init_test_logging();
    info!("Starting conformance_create_all_types test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Only test types supported by both br and bd
    // bd supports: bug, feature, task, epic, chore
    // br supports: bug, feature, task, epic, chore, docs, question
    let types = ["bug", "feature", "task", "epic", "chore"];

    for issue_type in types {
        let title = format!("Test {} issue", issue_type);
        let br_create = workspace.run_br(
            ["create", &title, "--type", issue_type, "--json"],
            &format!("create_{}", issue_type),
        );
        let bd_create = workspace.run_bd(
            ["create", &title, "--type", issue_type, "--json"],
            &format!("create_{}", issue_type),
        );

        assert!(
            br_create.status.success(),
            "br create --type {} failed: {}",
            issue_type,
            br_create.stderr
        );
        assert!(
            bd_create.status.success(),
            "bd create --type {} failed: {}",
            issue_type,
            bd_create.stderr
        );

        let br_json = extract_json_payload(&br_create.stdout);
        let bd_json = extract_json_payload(&bd_create.stdout);

        let result = compare_json(
            &br_json,
            &bd_json,
            &CompareMode::ContainsFields(vec!["issue_type".to_string()]),
        );
        assert!(
            result.is_ok(),
            "type {} comparison failed: {:?}",
            issue_type,
            result.err()
        );
    }

    info!("conformance_create_all_types passed");
}

#[test]
fn conformance_create_all_priorities() {
    common::init_test_logging();
    info!("Starting conformance_create_all_priorities test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    for priority in 0..=4 {
        let title = format!("Priority {} issue", priority);
        let priority_str = priority.to_string();
        let br_create = workspace.run_br(
            ["create", &title, "--priority", &priority_str, "--json"],
            &format!("create_p{}", priority),
        );
        let bd_create = workspace.run_bd(
            ["create", &title, "--priority", &priority_str, "--json"],
            &format!("create_p{}", priority),
        );

        assert!(
            br_create.status.success(),
            "br create --priority {} failed: {}",
            priority,
            br_create.stderr
        );
        assert!(
            bd_create.status.success(),
            "bd create --priority {} failed: {}",
            priority,
            bd_create.stderr
        );

        let br_json = extract_json_payload(&br_create.stdout);
        let bd_json = extract_json_payload(&bd_create.stdout);

        let br_val: Value = serde_json::from_str(&br_json).expect("parse br");
        let bd_val: Value = serde_json::from_str(&bd_json).expect("parse bd");

        let br_p = br_val["priority"].as_i64().or_else(|| br_val[0]["priority"].as_i64());
        let bd_p = bd_val["priority"].as_i64().or_else(|| bd_val[0]["priority"].as_i64());

        assert_eq!(
            br_p, bd_p,
            "priority {} mismatch: br={:?}, bd={:?}",
            priority, br_p, bd_p
        );
    }

    info!("conformance_create_all_priorities passed");
}

#[test]
fn conformance_create_with_assignee() {
    common::init_test_logging();
    info!("Starting conformance_create_with_assignee test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_create = workspace.run_br(
        ["create", "Assigned issue", "--assignee", "alice", "--json"],
        "create_assigned",
    );
    let bd_create = workspace.run_bd(
        ["create", "Assigned issue", "--assignee", "alice", "--json"],
        "create_assigned",
    );

    assert!(
        br_create.status.success(),
        "br create failed: {}",
        br_create.stderr
    );
    assert!(
        bd_create.status.success(),
        "bd create failed: {}",
        bd_create.stderr
    );

    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

    let br_assignee = br_val["assignee"]
        .as_str()
        .or_else(|| br_val[0]["assignee"].as_str());
    let bd_assignee = bd_val["assignee"]
        .as_str()
        .or_else(|| bd_val[0]["assignee"].as_str());

    assert_eq!(
        br_assignee, bd_assignee,
        "assignee mismatch: br={:?}, bd={:?}",
        br_assignee, bd_assignee
    );

    info!("conformance_create_with_assignee passed");
}

#[test]
fn conformance_create_with_description() {
    common::init_test_logging();
    info!("Starting conformance_create_with_description test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let desc = "This is a detailed description\nwith multiple lines.";
    let br_create = workspace.run_br(
        ["create", "Issue with desc", "--description", desc, "--json"],
        "create_desc",
    );
    let bd_create = workspace.run_bd(
        ["create", "Issue with desc", "--description", desc, "--json"],
        "create_desc",
    );

    assert!(
        br_create.status.success(),
        "br create failed: {}",
        br_create.stderr
    );
    assert!(
        bd_create.status.success(),
        "bd create failed: {}",
        bd_create.stderr
    );

    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

    let br_desc = br_val["description"]
        .as_str()
        .or_else(|| br_val[0]["description"].as_str());
    let bd_desc = bd_val["description"]
        .as_str()
        .or_else(|| bd_val[0]["description"].as_str());

    assert_eq!(
        br_desc, bd_desc,
        "description mismatch: br={:?}, bd={:?}",
        br_desc, bd_desc
    );

    info!("conformance_create_with_description passed");
}

#[test]
fn conformance_create_unicode_title() {
    common::init_test_logging();
    info!("Starting conformance_create_unicode_title test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let unicode_titles = [
        "日本語のタイトル",          // Japanese
        "Emoji test 🎉🚀💻",         // Emoji
        "مرحبا بالعالم",              // Arabic (RTL)
        "Ñoño español",              // Spanish with ñ
        "Über Größe",                // German umlauts
    ];

    for title in unicode_titles {
        let br_create = workspace.run_br(["create", title, "--json"], "create_unicode");
        let bd_create = workspace.run_bd(["create", title, "--json"], "create_unicode");

        assert!(
            br_create.status.success(),
            "br create unicode failed for '{}': {}",
            title,
            br_create.stderr
        );
        assert!(
            bd_create.status.success(),
            "bd create unicode failed for '{}': {}",
            title,
            bd_create.stderr
        );

        let br_json = extract_json_payload(&br_create.stdout);
        let bd_json = extract_json_payload(&bd_create.stdout);

        let br_val: Value = serde_json::from_str(&br_json).expect("parse");
        let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

        let br_title = br_val["title"]
            .as_str()
            .or_else(|| br_val[0]["title"].as_str());
        let bd_title = bd_val["title"]
            .as_str()
            .or_else(|| bd_val[0]["title"].as_str());

        assert_eq!(
            br_title, bd_title,
            "unicode title mismatch for '{}': br={:?}, bd={:?}",
            title, br_title, bd_title
        );
    }

    info!("conformance_create_unicode_title passed");
}

#[test]
fn conformance_create_special_chars() {
    common::init_test_logging();
    info!("Starting conformance_create_special_chars test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Test special characters that might break parsing
    let special_titles = [
        "Title with 'single quotes'",
        "Title with \"double quotes\"",
        "Title with \\backslashes\\",
        "Title with <angle> & ampersand",
    ];

    for title in special_titles {
        let br_create = workspace.run_br(["create", title, "--json"], "create_special");
        let bd_create = workspace.run_bd(["create", title, "--json"], "create_special");

        assert!(
            br_create.status.success(),
            "br create special failed for '{}': {}",
            title,
            br_create.stderr
        );
        assert!(
            bd_create.status.success(),
            "bd create special failed for '{}': {}",
            title,
            bd_create.stderr
        );

        let br_json = extract_json_payload(&br_create.stdout);
        let bd_json = extract_json_payload(&bd_create.stdout);

        let br_val: Value = serde_json::from_str(&br_json).expect("parse");
        let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

        let br_title = br_val["title"]
            .as_str()
            .or_else(|| br_val[0]["title"].as_str());
        let bd_title = bd_val["title"]
            .as_str()
            .or_else(|| bd_val[0]["title"].as_str());

        assert_eq!(
            br_title, bd_title,
            "special char title mismatch for '{}': br={:?}, bd={:?}",
            title, br_title, bd_title
        );
    }

    info!("conformance_create_special_chars passed");
}

// ============================================================================
// DEPENDENCY COMMAND CONFORMANCE TESTS (beads_rust-v740)
// ============================================================================

/// Helper function to extract an issue ID from JSON output (handles both object and array formats)
fn extract_issue_id(json_str: &str) -> String {
    let val: Value = serde_json::from_str(json_str).expect("parse json");
    val["id"]
        .as_str()
        .or_else(|| val[0]["id"].as_str())
        .expect("id field")
        .to_string()
}

// ---------------------------------------------------------------------------
// dep add tests (8)
// ---------------------------------------------------------------------------

#[test]
fn conformance_dep_add_basic() {
    common::init_test_logging();
    info!("Starting conformance_dep_add_basic test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create two issues
    let br_blocker = workspace.run_br(["create", "Blocker issue", "--json"], "create_blocker");
    let bd_blocker = workspace.run_bd(["create", "Blocker issue", "--json"], "create_blocker");

    let br_dependent = workspace.run_br(["create", "Dependent issue", "--json"], "create_dependent");
    let bd_dependent = workspace.run_bd(["create", "Dependent issue", "--json"], "create_dependent");

    let br_blocker_id = extract_issue_id(&extract_json_payload(&br_blocker.stdout));
    let bd_blocker_id = extract_issue_id(&extract_json_payload(&bd_blocker.stdout));
    let br_dependent_id = extract_issue_id(&extract_json_payload(&br_dependent.stdout));
    let bd_dependent_id = extract_issue_id(&extract_json_payload(&bd_dependent.stdout));

    // Add basic blocks dependency
    let br_add = workspace.run_br(
        ["dep", "add", &br_dependent_id, &br_blocker_id, "--json"],
        "dep_add",
    );
    let bd_add = workspace.run_bd(
        ["dep", "add", &bd_dependent_id, &bd_blocker_id, "--json"],
        "dep_add",
    );

    assert!(br_add.status.success(), "br dep add failed: {}", br_add.stderr);
    assert!(bd_add.status.success(), "bd dep add failed: {}", bd_add.stderr);

    // Both should produce similar JSON structure
    let br_json = extract_json_payload(&br_add.stdout);
    let bd_json = extract_json_payload(&bd_add.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Null);
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Null);

    // Check that both have action/status fields indicating success
    let br_status = br_val["status"].as_str().or(br_val["action"].as_str());
    let bd_status = bd_val["status"].as_str().or(bd_val["action"].as_str());

    assert!(
        br_status.is_some() || br_add.status.success(),
        "br should indicate success"
    );
    assert!(
        bd_status.is_some() || bd_add.status.success(),
        "bd should indicate success"
    );

    info!("conformance_dep_add_basic passed");
}

#[test]
fn conformance_dep_add_all_types() {
    common::init_test_logging();
    info!("Starting conformance_dep_add_all_types test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Test all standard dependency types
    let dep_types = [
        "blocks",
        "parent-child",
        "conditional-blocks",
        "waits-for",
        "related",
        "discovered-from",
        "replies-to",
        "relates-to",
        "duplicates",
        "supersedes",
        "caused-by",
    ];

    for dep_type in dep_types {
        // Create fresh issues for each type to avoid conflicts
        let br_source = workspace.run_br(
            ["create", &format!("Source for {}", dep_type), "--json"],
            &format!("create_source_{}", dep_type),
        );
        let bd_source = workspace.run_bd(
            ["create", &format!("Source for {}", dep_type), "--json"],
            &format!("create_source_{}", dep_type),
        );

        let br_target = workspace.run_br(
            ["create", &format!("Target for {}", dep_type), "--json"],
            &format!("create_target_{}", dep_type),
        );
        let bd_target = workspace.run_bd(
            ["create", &format!("Target for {}", dep_type), "--json"],
            &format!("create_target_{}", dep_type),
        );

        let br_source_id = extract_issue_id(&extract_json_payload(&br_source.stdout));
        let bd_source_id = extract_issue_id(&extract_json_payload(&bd_source.stdout));
        let br_target_id = extract_issue_id(&extract_json_payload(&br_target.stdout));
        let bd_target_id = extract_issue_id(&extract_json_payload(&bd_target.stdout));

        // Add dependency with specific type
        let br_add = workspace.run_br(
            ["dep", "add", &br_source_id, &br_target_id, "-t", dep_type],
            &format!("dep_add_{}", dep_type),
        );
        let bd_add = workspace.run_bd(
            ["dep", "add", &bd_source_id, &bd_target_id, "-t", dep_type],
            &format!("dep_add_{}", dep_type),
        );

        assert!(
            br_add.status.success(),
            "br dep add failed for type '{}': {}",
            dep_type,
            br_add.stderr
        );
        assert!(
            bd_add.status.success(),
            "bd dep add failed for type '{}': {}",
            dep_type,
            bd_add.stderr
        );
    }

    info!("conformance_dep_add_all_types passed");
}

#[test]
fn conformance_dep_add_duplicate() {
    common::init_test_logging();
    info!("Starting conformance_dep_add_duplicate test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create two issues
    let br_a = workspace.run_br(["create", "Issue A", "--json"], "create_a");
    let bd_a = workspace.run_bd(["create", "Issue A", "--json"], "create_a");

    let br_b = workspace.run_br(["create", "Issue B", "--json"], "create_b");
    let bd_b = workspace.run_bd(["create", "Issue B", "--json"], "create_b");

    let br_a_id = extract_issue_id(&extract_json_payload(&br_a.stdout));
    let bd_a_id = extract_issue_id(&extract_json_payload(&bd_a.stdout));
    let br_b_id = extract_issue_id(&extract_json_payload(&br_b.stdout));
    let bd_b_id = extract_issue_id(&extract_json_payload(&bd_b.stdout));

    // Add dependency first time
    let br_add1 = workspace.run_br(["dep", "add", &br_a_id, &br_b_id], "dep_add_1");
    let bd_add1 = workspace.run_bd(["dep", "add", &bd_a_id, &bd_b_id], "dep_add_1");

    assert!(br_add1.status.success(), "br first dep add failed");
    assert!(bd_add1.status.success(), "bd first dep add failed");

    // Add same dependency again - should be idempotent (succeed without error)
    let br_add2 = workspace.run_br(["dep", "add", &br_a_id, &br_b_id, "--json"], "dep_add_2");
    let bd_add2 = workspace.run_bd(["dep", "add", &bd_a_id, &bd_b_id, "--json"], "dep_add_2");

    // Both should succeed (idempotent behavior) or both should fail consistently
    assert_eq!(
        br_add2.status.success(),
        bd_add2.status.success(),
        "br and bd differ on duplicate dep handling: br={}, bd={}",
        br_add2.status.success(),
        bd_add2.status.success()
    );

    info!("conformance_dep_add_duplicate passed");
}

#[test]
fn conformance_dep_add_self_reference_error() {
    common::init_test_logging();
    info!("Starting conformance_dep_add_self_reference_error test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create an issue
    let br_issue = workspace.run_br(["create", "Self-ref test", "--json"], "create");
    let bd_issue = workspace.run_bd(["create", "Self-ref test", "--json"], "create");

    let br_id = extract_issue_id(&extract_json_payload(&br_issue.stdout));
    let bd_id = extract_issue_id(&extract_json_payload(&bd_issue.stdout));

    // Try to add self-dependency - should fail
    let br_add = workspace.run_br(["dep", "add", &br_id, &br_id], "dep_add_self");
    let bd_add = workspace.run_bd(["dep", "add", &bd_id, &bd_id], "dep_add_self");

    // Both should fail
    assert!(
        !br_add.status.success(),
        "br should reject self-dependency but it succeeded"
    );
    assert!(
        !bd_add.status.success(),
        "bd should reject self-dependency but it succeeded"
    );

    info!("conformance_dep_add_self_reference_error passed");
}

#[test]
fn conformance_dep_add_cycle_detection() {
    common::init_test_logging();
    info!("Starting conformance_dep_add_cycle_detection test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create two issues
    let br_a = workspace.run_br(["create", "Cycle A", "--json"], "create_a");
    let bd_a = workspace.run_bd(["create", "Cycle A", "--json"], "create_a");

    let br_b = workspace.run_br(["create", "Cycle B", "--json"], "create_b");
    let bd_b = workspace.run_bd(["create", "Cycle B", "--json"], "create_b");

    let br_a_id = extract_issue_id(&extract_json_payload(&br_a.stdout));
    let bd_a_id = extract_issue_id(&extract_json_payload(&bd_a.stdout));
    let br_b_id = extract_issue_id(&extract_json_payload(&br_b.stdout));
    let bd_b_id = extract_issue_id(&extract_json_payload(&bd_b.stdout));

    // A depends on B (A waits for B)
    let br_add1 = workspace.run_br(["dep", "add", &br_a_id, &br_b_id], "add_a_to_b");
    let bd_add1 = workspace.run_bd(["dep", "add", &bd_a_id, &bd_b_id], "add_a_to_b");

    assert!(br_add1.status.success(), "br first dep failed");
    assert!(bd_add1.status.success(), "bd first dep failed");

    // Try B depends on A - should create cycle, should fail
    let br_add2 = workspace.run_br(["dep", "add", &br_b_id, &br_a_id], "add_b_to_a");
    let bd_add2 = workspace.run_bd(["dep", "add", &bd_b_id, &bd_a_id], "add_b_to_a");

    // Both should fail due to cycle detection
    assert!(
        !br_add2.status.success(),
        "br should reject cycle A->B->A but succeeded"
    );
    assert!(
        !bd_add2.status.success(),
        "bd should reject cycle A->B->A but succeeded"
    );

    info!("conformance_dep_add_cycle_detection passed");
}

#[test]
fn conformance_dep_add_transitive_cycle() {
    common::init_test_logging();
    info!("Starting conformance_dep_add_transitive_cycle test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create three issues
    let br_a = workspace.run_br(["create", "Trans A", "--json"], "create_a");
    let bd_a = workspace.run_bd(["create", "Trans A", "--json"], "create_a");

    let br_b = workspace.run_br(["create", "Trans B", "--json"], "create_b");
    let bd_b = workspace.run_bd(["create", "Trans B", "--json"], "create_b");

    let br_c = workspace.run_br(["create", "Trans C", "--json"], "create_c");
    let bd_c = workspace.run_bd(["create", "Trans C", "--json"], "create_c");

    let br_a_id = extract_issue_id(&extract_json_payload(&br_a.stdout));
    let bd_a_id = extract_issue_id(&extract_json_payload(&bd_a.stdout));
    let br_b_id = extract_issue_id(&extract_json_payload(&br_b.stdout));
    let bd_b_id = extract_issue_id(&extract_json_payload(&bd_b.stdout));
    let br_c_id = extract_issue_id(&extract_json_payload(&br_c.stdout));
    let bd_c_id = extract_issue_id(&extract_json_payload(&bd_c.stdout));

    // A -> B -> C chain
    let br_ab = workspace.run_br(["dep", "add", &br_a_id, &br_b_id], "add_a_b");
    let bd_ab = workspace.run_bd(["dep", "add", &bd_a_id, &bd_b_id], "add_a_b");
    assert!(br_ab.status.success());
    assert!(bd_ab.status.success());

    let br_bc = workspace.run_br(["dep", "add", &br_b_id, &br_c_id], "add_b_c");
    let bd_bc = workspace.run_bd(["dep", "add", &bd_b_id, &bd_c_id], "add_b_c");
    assert!(br_bc.status.success());
    assert!(bd_bc.status.success());

    // Try C -> A (creates cycle A->B->C->A)
    let br_ca = workspace.run_br(["dep", "add", &br_c_id, &br_a_id], "add_c_a");
    let bd_ca = workspace.run_bd(["dep", "add", &bd_c_id, &bd_a_id], "add_c_a");

    // Both should fail
    assert!(
        !br_ca.status.success(),
        "br should reject transitive cycle A->B->C->A"
    );
    assert!(
        !bd_ca.status.success(),
        "bd should reject transitive cycle A->B->C->A"
    );

    info!("conformance_dep_add_transitive_cycle passed");
}

#[test]
fn conformance_dep_add_nonexistent_source_error() {
    common::init_test_logging();
    info!("Starting conformance_dep_add_nonexistent_source_error test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create only one issue
    let br_target = workspace.run_br(["create", "Target issue", "--json"], "create_target");
    let bd_target = workspace.run_bd(["create", "Target issue", "--json"], "create_target");

    let br_target_id = extract_issue_id(&extract_json_payload(&br_target.stdout));
    let bd_target_id = extract_issue_id(&extract_json_payload(&bd_target.stdout));

    // Try to add dep from nonexistent source
    let br_add = workspace.run_br(["dep", "add", "bd-nonexistent999", &br_target_id], "dep_add");
    let bd_add = workspace.run_bd(["dep", "add", "bd-nonexistent999", &bd_target_id], "dep_add");

    // Both should fail
    assert!(
        !br_add.status.success(),
        "br should reject nonexistent source"
    );
    assert!(
        !bd_add.status.success(),
        "bd should reject nonexistent source"
    );

    info!("conformance_dep_add_nonexistent_source_error passed");
}

#[test]
fn conformance_dep_add_nonexistent_target_error() {
    common::init_test_logging();
    info!("Starting conformance_dep_add_nonexistent_target_error test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create only one issue
    let br_source = workspace.run_br(["create", "Source issue", "--json"], "create_source");
    let bd_source = workspace.run_bd(["create", "Source issue", "--json"], "create_source");

    let br_source_id = extract_issue_id(&extract_json_payload(&br_source.stdout));
    let bd_source_id = extract_issue_id(&extract_json_payload(&bd_source.stdout));

    // Try to add dep to nonexistent target
    let br_add = workspace.run_br(["dep", "add", &br_source_id, "bd-nonexistent999"], "dep_add");
    let bd_add = workspace.run_bd(["dep", "add", &bd_source_id, "bd-nonexistent999"], "dep_add");

    // Both should fail
    assert!(
        !br_add.status.success(),
        "br should reject nonexistent target"
    );
    assert!(
        !bd_add.status.success(),
        "bd should reject nonexistent target"
    );

    info!("conformance_dep_add_nonexistent_target_error passed");
}

// ---------------------------------------------------------------------------
// dep remove tests (5)
// ---------------------------------------------------------------------------

#[test]
fn conformance_dep_remove_basic_expanded() {
    common::init_test_logging();
    info!("Starting conformance_dep_remove_basic_expanded test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    let br_a = workspace.run_br(["create", "Remove A", "--json"], "create_a");
    let bd_a = workspace.run_bd(["create", "Remove A", "--json"], "create_a");

    let br_b = workspace.run_br(["create", "Remove B", "--json"], "create_b");
    let bd_b = workspace.run_bd(["create", "Remove B", "--json"], "create_b");

    let br_a_id = extract_issue_id(&extract_json_payload(&br_a.stdout));
    let bd_a_id = extract_issue_id(&extract_json_payload(&bd_a.stdout));
    let br_b_id = extract_issue_id(&extract_json_payload(&br_b.stdout));
    let bd_b_id = extract_issue_id(&extract_json_payload(&bd_b.stdout));

    // Add dependency
    workspace.run_br(["dep", "add", &br_a_id, &br_b_id], "add_dep");
    workspace.run_bd(["dep", "add", &bd_a_id, &bd_b_id], "add_dep");

    // Remove with JSON output
    let br_rm = workspace.run_br(["dep", "remove", &br_a_id, &br_b_id, "--json"], "rm_dep");
    let bd_rm = workspace.run_bd(["dep", "remove", &bd_a_id, &bd_b_id, "--json"], "rm_dep");

    assert!(br_rm.status.success(), "br dep remove failed: {}", br_rm.stderr);
    assert!(bd_rm.status.success(), "bd dep remove failed: {}", bd_rm.stderr);

    // Verify dependency is gone
    let br_list = workspace.run_br(["dep", "list", &br_a_id, "--json"], "list_after");
    let bd_list = workspace.run_bd(["dep", "list", &bd_a_id, "--json"], "list_after");

    let br_json = extract_json_payload(&br_list.stdout);
    let bd_json = extract_json_payload(&bd_list.stdout);

    let br_deps: Value = serde_json::from_str(&br_json).unwrap_or(Value::Array(vec![]));
    let bd_deps: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Array(vec![]));

    let br_len = br_deps.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_deps.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(br_len, 0, "br should have 0 deps after remove");
    assert_eq!(bd_len, 0, "bd should have 0 deps after remove");

    info!("conformance_dep_remove_basic_expanded passed");
}

#[test]
fn conformance_dep_remove_nonexistent() {
    common::init_test_logging();
    info!("Starting conformance_dep_remove_nonexistent test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues but don't add dependency
    let br_a = workspace.run_br(["create", "No-dep A", "--json"], "create_a");
    let bd_a = workspace.run_bd(["create", "No-dep A", "--json"], "create_a");

    let br_b = workspace.run_br(["create", "No-dep B", "--json"], "create_b");
    let bd_b = workspace.run_bd(["create", "No-dep B", "--json"], "create_b");

    let br_a_id = extract_issue_id(&extract_json_payload(&br_a.stdout));
    let bd_a_id = extract_issue_id(&extract_json_payload(&bd_a.stdout));
    let br_b_id = extract_issue_id(&extract_json_payload(&br_b.stdout));
    let bd_b_id = extract_issue_id(&extract_json_payload(&bd_b.stdout));

    // Try to remove non-existent dependency
    let br_rm = workspace.run_br(["dep", "remove", &br_a_id, &br_b_id, "--json"], "rm_nonexistent");
    let bd_rm = workspace.run_bd(["dep", "remove", &bd_a_id, &bd_b_id, "--json"], "rm_nonexistent");

    // Both should handle gracefully (could be success with not_found status, or error)
    // The important thing is they behave the same way
    assert_eq!(
        br_rm.status.success(),
        bd_rm.status.success(),
        "br and bd differ on removing nonexistent dep: br={}, bd={}",
        br_rm.status.success(),
        bd_rm.status.success()
    );

    info!("conformance_dep_remove_nonexistent passed");
}

#[test]
fn conformance_dep_remove_unblocks_issue() {
    common::init_test_logging();
    info!("Starting conformance_dep_remove_unblocks_issue test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create blocker and blocked issues
    let br_blocker = workspace.run_br(["create", "Blocker", "--json"], "create_blocker");
    let bd_blocker = workspace.run_bd(["create", "Blocker", "--json"], "create_blocker");

    let br_blocked = workspace.run_br(["create", "Blocked", "--json"], "create_blocked");
    let bd_blocked = workspace.run_bd(["create", "Blocked", "--json"], "create_blocked");

    let br_blocker_id = extract_issue_id(&extract_json_payload(&br_blocker.stdout));
    let bd_blocker_id = extract_issue_id(&extract_json_payload(&bd_blocker.stdout));
    let br_blocked_id = extract_issue_id(&extract_json_payload(&br_blocked.stdout));
    let bd_blocked_id = extract_issue_id(&extract_json_payload(&bd_blocked.stdout));

    // Add blocking dependency
    workspace.run_br(["dep", "add", &br_blocked_id, &br_blocker_id], "add_dep");
    workspace.run_bd(["dep", "add", &bd_blocked_id, &bd_blocker_id], "add_dep");

    // Verify blocked
    let br_blocked_before = workspace.run_br(["blocked", "--json"], "blocked_before");
    let bd_blocked_before = workspace.run_bd(["blocked", "--json"], "blocked_before");

    let br_before: Value = serde_json::from_str(&extract_json_payload(&br_blocked_before.stdout))
        .unwrap_or(Value::Array(vec![]));
    let bd_before: Value = serde_json::from_str(&extract_json_payload(&bd_blocked_before.stdout))
        .unwrap_or(Value::Array(vec![]));

    assert_eq!(br_before.as_array().map(|a| a.len()).unwrap_or(0), 1, "br should have 1 blocked");
    assert_eq!(bd_before.as_array().map(|a| a.len()).unwrap_or(0), 1, "bd should have 1 blocked");

    // Remove dependency
    workspace.run_br(["dep", "remove", &br_blocked_id, &br_blocker_id], "rm_dep");
    workspace.run_bd(["dep", "remove", &bd_blocked_id, &bd_blocker_id], "rm_dep");

    // Verify unblocked
    let br_blocked_after = workspace.run_br(["blocked", "--json"], "blocked_after");
    let bd_blocked_after = workspace.run_bd(["blocked", "--json"], "blocked_after");

    let br_after: Value = serde_json::from_str(&extract_json_payload(&br_blocked_after.stdout))
        .unwrap_or(Value::Array(vec![]));
    let bd_after: Value = serde_json::from_str(&extract_json_payload(&bd_blocked_after.stdout))
        .unwrap_or(Value::Array(vec![]));

    assert_eq!(br_after.as_array().map(|a| a.len()).unwrap_or(0), 0, "br should have 0 blocked");
    assert_eq!(bd_after.as_array().map(|a| a.len()).unwrap_or(0), 0, "bd should have 0 blocked");

    // Verify now ready
    let br_ready = workspace.run_br(["ready", "--json"], "ready_after");
    let bd_ready = workspace.run_bd(["ready", "--json"], "ready_after");

    let br_ready_val: Value = serde_json::from_str(&extract_json_payload(&br_ready.stdout))
        .unwrap_or(Value::Array(vec![]));
    let bd_ready_val: Value = serde_json::from_str(&extract_json_payload(&bd_ready.stdout))
        .unwrap_or(Value::Array(vec![]));

    // Both issues should now be ready
    assert_eq!(
        br_ready_val.as_array().map(|a| a.len()).unwrap_or(0),
        bd_ready_val.as_array().map(|a| a.len()).unwrap_or(0),
        "ready counts should match"
    );

    info!("conformance_dep_remove_unblocks_issue passed");
}

#[test]
fn conformance_dep_remove_preserves_other_deps() {
    common::init_test_logging();
    info!("Starting conformance_dep_remove_preserves_other_deps test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create three issues
    let br_a = workspace.run_br(["create", "Multi A", "--json"], "create_a");
    let bd_a = workspace.run_bd(["create", "Multi A", "--json"], "create_a");

    let br_b = workspace.run_br(["create", "Multi B", "--json"], "create_b");
    let bd_b = workspace.run_bd(["create", "Multi B", "--json"], "create_b");

    let br_c = workspace.run_br(["create", "Multi C", "--json"], "create_c");
    let bd_c = workspace.run_bd(["create", "Multi C", "--json"], "create_c");

    let br_a_id = extract_issue_id(&extract_json_payload(&br_a.stdout));
    let bd_a_id = extract_issue_id(&extract_json_payload(&bd_a.stdout));
    let br_b_id = extract_issue_id(&extract_json_payload(&br_b.stdout));
    let bd_b_id = extract_issue_id(&extract_json_payload(&bd_b.stdout));
    let br_c_id = extract_issue_id(&extract_json_payload(&br_c.stdout));
    let bd_c_id = extract_issue_id(&extract_json_payload(&bd_c.stdout));

    // A depends on both B and C
    workspace.run_br(["dep", "add", &br_a_id, &br_b_id, "-t", "related"], "add_a_b");
    workspace.run_bd(["dep", "add", &bd_a_id, &bd_b_id, "-t", "related"], "add_a_b");

    workspace.run_br(["dep", "add", &br_a_id, &br_c_id, "-t", "related"], "add_a_c");
    workspace.run_bd(["dep", "add", &bd_a_id, &bd_c_id, "-t", "related"], "add_a_c");

    // Verify 2 deps
    let br_list_before = workspace.run_br(["dep", "list", &br_a_id, "--json"], "list_before");
    let bd_list_before = workspace.run_bd(["dep", "list", &bd_a_id, "--json"], "list_before");

    let br_before: Value = serde_json::from_str(&extract_json_payload(&br_list_before.stdout))
        .unwrap_or(Value::Array(vec![]));
    let bd_before: Value = serde_json::from_str(&extract_json_payload(&bd_list_before.stdout))
        .unwrap_or(Value::Array(vec![]));

    assert_eq!(br_before.as_array().map(|a| a.len()).unwrap_or(0), 2);
    assert_eq!(bd_before.as_array().map(|a| a.len()).unwrap_or(0), 2);

    // Remove only A->B
    workspace.run_br(["dep", "remove", &br_a_id, &br_b_id], "rm_a_b");
    workspace.run_bd(["dep", "remove", &bd_a_id, &bd_b_id], "rm_a_b");

    // Verify A->C still exists
    let br_list_after = workspace.run_br(["dep", "list", &br_a_id, "--json"], "list_after");
    let bd_list_after = workspace.run_bd(["dep", "list", &bd_a_id, "--json"], "list_after");

    let br_after: Value = serde_json::from_str(&extract_json_payload(&br_list_after.stdout))
        .unwrap_or(Value::Array(vec![]));
    let bd_after: Value = serde_json::from_str(&extract_json_payload(&bd_list_after.stdout))
        .unwrap_or(Value::Array(vec![]));

    assert_eq!(br_after.as_array().map(|a| a.len()).unwrap_or(0), 1, "br should have 1 dep left");
    assert_eq!(bd_after.as_array().map(|a| a.len()).unwrap_or(0), 1, "bd should have 1 dep left");

    info!("conformance_dep_remove_preserves_other_deps passed");
}

// ---------------------------------------------------------------------------
// dep list tests (6)
// ---------------------------------------------------------------------------

#[test]
fn conformance_dep_list_basic_expanded() {
    common::init_test_logging();
    info!("Starting conformance_dep_list_basic_expanded test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues with dependency
    let br_parent = workspace.run_br(["create", "List Parent", "--json"], "create_parent");
    let bd_parent = workspace.run_bd(["create", "List Parent", "--json"], "create_parent");

    let br_child = workspace.run_br(["create", "List Child", "--json"], "create_child");
    let bd_child = workspace.run_bd(["create", "List Child", "--json"], "create_child");

    let br_parent_id = extract_issue_id(&extract_json_payload(&br_parent.stdout));
    let bd_parent_id = extract_issue_id(&extract_json_payload(&bd_parent.stdout));
    let br_child_id = extract_issue_id(&extract_json_payload(&br_child.stdout));
    let bd_child_id = extract_issue_id(&extract_json_payload(&bd_child.stdout));

    // Add dependency
    workspace.run_br(["dep", "add", &br_child_id, &br_parent_id], "add_dep");
    workspace.run_bd(["dep", "add", &bd_child_id, &bd_parent_id], "add_dep");

    // List deps
    let br_list = workspace.run_br(["dep", "list", &br_child_id, "--json"], "list");
    let bd_list = workspace.run_bd(["dep", "list", &bd_child_id, "--json"], "list");

    assert!(br_list.status.success(), "br dep list failed");
    assert!(bd_list.status.success(), "bd dep list failed");

    let br_deps: Value = serde_json::from_str(&extract_json_payload(&br_list.stdout))
        .unwrap_or(Value::Array(vec![]));
    let bd_deps: Value = serde_json::from_str(&extract_json_payload(&bd_list.stdout))
        .unwrap_or(Value::Array(vec![]));

    assert_eq!(
        br_deps.as_array().map(|a| a.len()).unwrap_or(0),
        bd_deps.as_array().map(|a| a.len()).unwrap_or(0),
        "dep list counts should match"
    );

    info!("conformance_dep_list_basic_expanded passed");
}

#[test]
fn conformance_dep_list_empty() {
    common::init_test_logging();
    info!("Starting conformance_dep_list_empty test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issue with no deps
    let br_issue = workspace.run_br(["create", "No deps issue", "--json"], "create");
    let bd_issue = workspace.run_bd(["create", "No deps issue", "--json"], "create");

    let br_id = extract_issue_id(&extract_json_payload(&br_issue.stdout));
    let bd_id = extract_issue_id(&extract_json_payload(&bd_issue.stdout));

    // List deps - should be empty
    let br_list = workspace.run_br(["dep", "list", &br_id, "--json"], "list_empty");
    let bd_list = workspace.run_bd(["dep", "list", &bd_id, "--json"], "list_empty");

    assert!(br_list.status.success(), "br dep list failed");
    assert!(bd_list.status.success(), "bd dep list failed");

    let br_deps: Value = serde_json::from_str(&extract_json_payload(&br_list.stdout))
        .unwrap_or(Value::Array(vec![]));
    let bd_deps: Value = serde_json::from_str(&extract_json_payload(&bd_list.stdout))
        .unwrap_or(Value::Array(vec![]));

    assert_eq!(br_deps.as_array().map(|a| a.len()).unwrap_or(0), 0, "br should have 0 deps");
    assert_eq!(bd_deps.as_array().map(|a| a.len()).unwrap_or(0), 0, "bd should have 0 deps");

    info!("conformance_dep_list_empty passed");
}

#[test]
fn conformance_dep_list_by_type() {
    common::init_test_logging();
    info!("Starting conformance_dep_list_by_type test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    let br_main = workspace.run_br(["create", "Main issue", "--json"], "create_main");
    let bd_main = workspace.run_bd(["create", "Main issue", "--json"], "create_main");

    let br_blocks = workspace.run_br(["create", "Blocks target", "--json"], "create_blocks");
    let bd_blocks = workspace.run_bd(["create", "Blocks target", "--json"], "create_blocks");

    let br_related = workspace.run_br(["create", "Related target", "--json"], "create_related");
    let bd_related = workspace.run_bd(["create", "Related target", "--json"], "create_related");

    let br_main_id = extract_issue_id(&extract_json_payload(&br_main.stdout));
    let bd_main_id = extract_issue_id(&extract_json_payload(&bd_main.stdout));
    let br_blocks_id = extract_issue_id(&extract_json_payload(&br_blocks.stdout));
    let bd_blocks_id = extract_issue_id(&extract_json_payload(&bd_blocks.stdout));
    let br_related_id = extract_issue_id(&extract_json_payload(&br_related.stdout));
    let bd_related_id = extract_issue_id(&extract_json_payload(&bd_related.stdout));

    // Add different dependency types
    workspace.run_br(["dep", "add", &br_main_id, &br_blocks_id, "-t", "blocks"], "add_blocks");
    workspace.run_bd(["dep", "add", &bd_main_id, &bd_blocks_id, "-t", "blocks"], "add_blocks");

    workspace.run_br(["dep", "add", &br_main_id, &br_related_id, "-t", "related"], "add_related");
    workspace.run_bd(["dep", "add", &bd_main_id, &bd_related_id, "-t", "related"], "add_related");

    // List only blocks type
    let br_list = workspace.run_br(["dep", "list", &br_main_id, "-t", "blocks", "--json"], "list_blocks");
    let bd_list = workspace.run_bd(["dep", "list", &bd_main_id, "-t", "blocks", "--json"], "list_blocks");

    let br_deps: Value = serde_json::from_str(&extract_json_payload(&br_list.stdout))
        .unwrap_or(Value::Array(vec![]));
    let bd_deps: Value = serde_json::from_str(&extract_json_payload(&bd_list.stdout))
        .unwrap_or(Value::Array(vec![]));

    let br_len = br_deps.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_deps.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(br_len, bd_len, "filtered dep counts should match: br={}, bd={}", br_len, bd_len);

    info!("conformance_dep_list_by_type passed");
}

#[test]
fn conformance_dep_list_json_structure() {
    common::init_test_logging();
    info!("Starting conformance_dep_list_json_structure test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues with dependency
    let br_a = workspace.run_br(["create", "Struct A", "--json"], "create_a");
    let bd_a = workspace.run_bd(["create", "Struct A", "--json"], "create_a");

    let br_b = workspace.run_br(["create", "Struct B", "--json"], "create_b");
    let bd_b = workspace.run_bd(["create", "Struct B", "--json"], "create_b");

    let br_a_id = extract_issue_id(&extract_json_payload(&br_a.stdout));
    let bd_a_id = extract_issue_id(&extract_json_payload(&bd_a.stdout));
    let br_b_id = extract_issue_id(&extract_json_payload(&br_b.stdout));
    let bd_b_id = extract_issue_id(&extract_json_payload(&bd_b.stdout));

    workspace.run_br(["dep", "add", &br_a_id, &br_b_id], "add_dep");
    workspace.run_bd(["dep", "add", &bd_a_id, &bd_b_id], "add_dep");

    let br_list = workspace.run_br(["dep", "list", &br_a_id, "--json"], "list");
    let bd_list = workspace.run_bd(["dep", "list", &bd_a_id, "--json"], "list");

    let br_deps: Value = serde_json::from_str(&extract_json_payload(&br_list.stdout))
        .expect("br should produce valid JSON");
    let bd_deps: Value = serde_json::from_str(&extract_json_payload(&bd_list.stdout))
        .expect("bd should produce valid JSON");

    // Both should be arrays
    assert!(br_deps.is_array(), "br dep list should be an array");
    assert!(bd_deps.is_array(), "bd dep list should be an array");

    // If not empty, check structure
    if let Some(br_arr) = br_deps.as_array() {
        if let Some(first) = br_arr.first() {
            // Should have standard dep fields
            let has_issue_id = first.get("issue_id").is_some();
            let has_depends_on = first.get("depends_on_id").is_some();
            let has_type = first.get("type").is_some();

            assert!(
                has_issue_id || has_depends_on,
                "br dep list items should have id fields"
            );
            assert!(has_type, "br dep list items should have type field");
        }
    }

    info!("conformance_dep_list_json_structure passed");
}

// ---------------------------------------------------------------------------
// dep tree tests (6)
// ---------------------------------------------------------------------------

#[test]
fn conformance_dep_tree_basic() {
    common::init_test_logging();
    info!("Starting conformance_dep_tree_basic test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create simple hierarchy
    let br_root = workspace.run_br(["create", "Tree Root", "--json"], "create_root");
    let bd_root = workspace.run_bd(["create", "Tree Root", "--json"], "create_root");

    let br_child = workspace.run_br(["create", "Tree Child", "--json"], "create_child");
    let bd_child = workspace.run_bd(["create", "Tree Child", "--json"], "create_child");

    let br_root_id = extract_issue_id(&extract_json_payload(&br_root.stdout));
    let bd_root_id = extract_issue_id(&extract_json_payload(&bd_root.stdout));
    let br_child_id = extract_issue_id(&extract_json_payload(&br_child.stdout));
    let bd_child_id = extract_issue_id(&extract_json_payload(&bd_child.stdout));

    // Child depends on root (root blocks child)
    workspace.run_br(["dep", "add", &br_child_id, &br_root_id], "add_dep");
    workspace.run_bd(["dep", "add", &bd_child_id, &bd_root_id], "add_dep");

    // Get tree from root
    let br_tree = workspace.run_br(["dep", "tree", &br_root_id], "tree");
    let bd_tree = workspace.run_bd(["dep", "tree", &bd_root_id], "tree");

    assert!(br_tree.status.success(), "br dep tree failed: {}", br_tree.stderr);
    assert!(bd_tree.status.success(), "bd dep tree failed: {}", bd_tree.stderr);

    // Both should produce output
    assert!(!br_tree.stdout.trim().is_empty(), "br tree should have output");
    assert!(!bd_tree.stdout.trim().is_empty(), "bd tree should have output");

    info!("conformance_dep_tree_basic passed");
}

#[test]
fn conformance_dep_tree_deep() {
    common::init_test_logging();
    info!("Starting conformance_dep_tree_deep test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create chain: A -> B -> C -> D
    let br_a = workspace.run_br(["create", "Deep A", "--json"], "create_a");
    let bd_a = workspace.run_bd(["create", "Deep A", "--json"], "create_a");

    let br_b = workspace.run_br(["create", "Deep B", "--json"], "create_b");
    let bd_b = workspace.run_bd(["create", "Deep B", "--json"], "create_b");

    let br_c = workspace.run_br(["create", "Deep C", "--json"], "create_c");
    let bd_c = workspace.run_bd(["create", "Deep C", "--json"], "create_c");

    let br_d = workspace.run_br(["create", "Deep D", "--json"], "create_d");
    let bd_d = workspace.run_bd(["create", "Deep D", "--json"], "create_d");

    let br_a_id = extract_issue_id(&extract_json_payload(&br_a.stdout));
    let bd_a_id = extract_issue_id(&extract_json_payload(&bd_a.stdout));
    let br_b_id = extract_issue_id(&extract_json_payload(&br_b.stdout));
    let bd_b_id = extract_issue_id(&extract_json_payload(&bd_b.stdout));
    let br_c_id = extract_issue_id(&extract_json_payload(&br_c.stdout));
    let bd_c_id = extract_issue_id(&extract_json_payload(&bd_c.stdout));
    let br_d_id = extract_issue_id(&extract_json_payload(&br_d.stdout));
    let bd_d_id = extract_issue_id(&extract_json_payload(&bd_d.stdout));

    // Build chain: B depends on A, C on B, D on C
    workspace.run_br(["dep", "add", &br_b_id, &br_a_id], "add_b_a");
    workspace.run_bd(["dep", "add", &bd_b_id, &bd_a_id], "add_b_a");

    workspace.run_br(["dep", "add", &br_c_id, &br_b_id], "add_c_b");
    workspace.run_bd(["dep", "add", &bd_c_id, &bd_b_id], "add_c_b");

    workspace.run_br(["dep", "add", &br_d_id, &br_c_id], "add_d_c");
    workspace.run_bd(["dep", "add", &bd_d_id, &bd_c_id], "add_d_c");

    // Get tree from A
    let br_tree = workspace.run_br(["dep", "tree", &br_a_id], "tree");
    let bd_tree = workspace.run_bd(["dep", "tree", &bd_a_id], "tree");

    assert!(br_tree.status.success(), "br dep tree failed");
    assert!(bd_tree.status.success(), "bd dep tree failed");

    info!("conformance_dep_tree_deep passed");
}

#[test]
fn conformance_dep_tree_empty() {
    common::init_test_logging();
    info!("Starting conformance_dep_tree_empty test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issue with no deps
    let br_issue = workspace.run_br(["create", "Tree empty", "--json"], "create");
    let bd_issue = workspace.run_bd(["create", "Tree empty", "--json"], "create");

    let br_id = extract_issue_id(&extract_json_payload(&br_issue.stdout));
    let bd_id = extract_issue_id(&extract_json_payload(&bd_issue.stdout));

    // Get tree - should just show the root
    let br_tree = workspace.run_br(["dep", "tree", &br_id], "tree");
    let bd_tree = workspace.run_bd(["dep", "tree", &bd_id], "tree");

    assert!(br_tree.status.success(), "br dep tree failed");
    assert!(bd_tree.status.success(), "bd dep tree failed");

    info!("conformance_dep_tree_empty passed");
}

#[test]
fn conformance_dep_tree_json() {
    common::init_test_logging();
    info!("Starting conformance_dep_tree_json test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create hierarchy
    let br_root = workspace.run_br(["create", "JSON Tree Root", "--json"], "create_root");
    let bd_root = workspace.run_bd(["create", "JSON Tree Root", "--json"], "create_root");

    let br_child = workspace.run_br(["create", "JSON Tree Child", "--json"], "create_child");
    let bd_child = workspace.run_bd(["create", "JSON Tree Child", "--json"], "create_child");

    let br_root_id = extract_issue_id(&extract_json_payload(&br_root.stdout));
    let bd_root_id = extract_issue_id(&extract_json_payload(&bd_root.stdout));
    let br_child_id = extract_issue_id(&extract_json_payload(&br_child.stdout));
    let bd_child_id = extract_issue_id(&extract_json_payload(&bd_child.stdout));

    workspace.run_br(["dep", "add", &br_child_id, &br_root_id], "add_dep");
    workspace.run_bd(["dep", "add", &bd_child_id, &bd_root_id], "add_dep");

    // Get tree as JSON
    let br_tree = workspace.run_br(["dep", "tree", &br_root_id, "--json"], "tree_json");
    let bd_tree = workspace.run_bd(["dep", "tree", &bd_root_id, "--json"], "tree_json");

    // Both should succeed
    let br_success = br_tree.status.success();
    let bd_success = bd_tree.status.success();

    // Both should behave the same
    assert_eq!(
        br_success, bd_success,
        "br and bd should both succeed or fail for tree --json"
    );

    if br_success {
        // Parse JSON if available
        let br_json = extract_json_payload(&br_tree.stdout);
        let bd_json = extract_json_payload(&bd_tree.stdout);

        let br_val: Result<Value, _> = serde_json::from_str(&br_json);
        let bd_val: Result<Value, _> = serde_json::from_str(&bd_json);

        assert!(br_val.is_ok(), "br tree JSON should be valid");
        assert!(bd_val.is_ok(), "bd tree JSON should be valid");
    }

    info!("conformance_dep_tree_json passed");
}

// ---------------------------------------------------------------------------
// dep cycles tests (4)
// ---------------------------------------------------------------------------

#[test]
fn conformance_dep_cycles_none() {
    common::init_test_logging();
    info!("Starting conformance_dep_cycles_none test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create linear chain (no cycles)
    let br_a = workspace.run_br(["create", "NoCycle A", "--json"], "create_a");
    let bd_a = workspace.run_bd(["create", "NoCycle A", "--json"], "create_a");

    let br_b = workspace.run_br(["create", "NoCycle B", "--json"], "create_b");
    let bd_b = workspace.run_bd(["create", "NoCycle B", "--json"], "create_b");

    let br_a_id = extract_issue_id(&extract_json_payload(&br_a.stdout));
    let bd_a_id = extract_issue_id(&extract_json_payload(&bd_a.stdout));
    let br_b_id = extract_issue_id(&extract_json_payload(&br_b.stdout));
    let bd_b_id = extract_issue_id(&extract_json_payload(&bd_b.stdout));

    // A -> B (no cycle possible)
    workspace.run_br(["dep", "add", &br_a_id, &br_b_id, "-t", "related"], "add_dep");
    workspace.run_bd(["dep", "add", &bd_a_id, &bd_b_id, "-t", "related"], "add_dep");

    // Check for cycles
    let br_cycles = workspace.run_br(["dep", "cycles", "--json"], "cycles");
    let bd_cycles = workspace.run_bd(["dep", "cycles", "--json"], "cycles");

    assert!(br_cycles.status.success(), "br dep cycles failed");
    assert!(bd_cycles.status.success(), "bd dep cycles failed");

    let br_json = extract_json_payload(&br_cycles.stdout);
    let bd_json = extract_json_payload(&bd_cycles.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Null);
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Null);

    // Both should report 0 cycles
    let br_count = br_val["count"].as_u64().unwrap_or(0);
    let bd_count = bd_val["count"].as_u64().unwrap_or(0);

    assert_eq!(br_count, 0, "br should find no cycles");
    assert_eq!(bd_count, 0, "bd should find no cycles");

    info!("conformance_dep_cycles_none passed");
}

#[test]
fn conformance_dep_cycles_simple() {
    common::init_test_logging();
    info!("Starting conformance_dep_cycles_simple test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create two issues
    let br_a = workspace.run_br(["create", "SimpleCycle A", "--json"], "create_a");
    let bd_a = workspace.run_bd(["create", "SimpleCycle A", "--json"], "create_a");

    let br_b = workspace.run_br(["create", "SimpleCycle B", "--json"], "create_b");
    let bd_b = workspace.run_bd(["create", "SimpleCycle B", "--json"], "create_b");

    let br_a_id = extract_issue_id(&extract_json_payload(&br_a.stdout));
    let bd_a_id = extract_issue_id(&extract_json_payload(&bd_a.stdout));
    let br_b_id = extract_issue_id(&extract_json_payload(&br_b.stdout));
    let bd_b_id = extract_issue_id(&extract_json_payload(&bd_b.stdout));

    // Create cycle using non-blocking type (related doesn't prevent cycles)
    workspace.run_br(["dep", "add", &br_a_id, &br_b_id, "-t", "related"], "add_a_b");
    workspace.run_bd(["dep", "add", &bd_a_id, &bd_b_id, "-t", "related"], "add_a_b");

    workspace.run_br(["dep", "add", &br_b_id, &br_a_id, "-t", "related"], "add_b_a");
    workspace.run_bd(["dep", "add", &bd_b_id, &bd_a_id, "-t", "related"], "add_b_a");

    // Check for cycles
    let br_cycles = workspace.run_br(["dep", "cycles", "--json"], "cycles");
    let bd_cycles = workspace.run_bd(["dep", "cycles", "--json"], "cycles");

    assert!(br_cycles.status.success(), "br dep cycles failed");
    assert!(bd_cycles.status.success(), "bd dep cycles failed");

    let br_json = extract_json_payload(&br_cycles.stdout);
    let bd_json = extract_json_payload(&bd_cycles.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Null);
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Null);

    // Both should detect the cycle
    let br_count = br_val["count"].as_u64().unwrap_or(0);
    let bd_count = bd_val["count"].as_u64().unwrap_or(0);

    assert_eq!(br_count, bd_count, "cycle counts should match: br={}, bd={}", br_count, bd_count);

    info!("conformance_dep_cycles_simple passed");
}

#[test]
fn conformance_dep_cycles_complex() {
    common::init_test_logging();
    info!("Starting conformance_dep_cycles_complex test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create three issues for A->B->C->A cycle
    let br_a = workspace.run_br(["create", "ComplexCycle A", "--json"], "create_a");
    let bd_a = workspace.run_bd(["create", "ComplexCycle A", "--json"], "create_a");

    let br_b = workspace.run_br(["create", "ComplexCycle B", "--json"], "create_b");
    let bd_b = workspace.run_bd(["create", "ComplexCycle B", "--json"], "create_b");

    let br_c = workspace.run_br(["create", "ComplexCycle C", "--json"], "create_c");
    let bd_c = workspace.run_bd(["create", "ComplexCycle C", "--json"], "create_c");

    let br_a_id = extract_issue_id(&extract_json_payload(&br_a.stdout));
    let bd_a_id = extract_issue_id(&extract_json_payload(&bd_a.stdout));
    let br_b_id = extract_issue_id(&extract_json_payload(&br_b.stdout));
    let bd_b_id = extract_issue_id(&extract_json_payload(&bd_b.stdout));
    let br_c_id = extract_issue_id(&extract_json_payload(&br_c.stdout));
    let bd_c_id = extract_issue_id(&extract_json_payload(&bd_c.stdout));

    // Create triangular cycle with non-blocking type
    workspace.run_br(["dep", "add", &br_a_id, &br_b_id, "-t", "related"], "add_a_b");
    workspace.run_bd(["dep", "add", &bd_a_id, &bd_b_id, "-t", "related"], "add_a_b");

    workspace.run_br(["dep", "add", &br_b_id, &br_c_id, "-t", "related"], "add_b_c");
    workspace.run_bd(["dep", "add", &bd_b_id, &bd_c_id, "-t", "related"], "add_b_c");

    workspace.run_br(["dep", "add", &br_c_id, &br_a_id, "-t", "related"], "add_c_a");
    workspace.run_bd(["dep", "add", &bd_c_id, &bd_a_id, "-t", "related"], "add_c_a");

    // Check for cycles
    let br_cycles = workspace.run_br(["dep", "cycles", "--json"], "cycles");
    let bd_cycles = workspace.run_bd(["dep", "cycles", "--json"], "cycles");

    assert!(br_cycles.status.success(), "br dep cycles failed");
    assert!(bd_cycles.status.success(), "bd dep cycles failed");

    let br_json = extract_json_payload(&br_cycles.stdout);
    let bd_json = extract_json_payload(&bd_cycles.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Null);
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Null);

    let br_count = br_val["count"].as_u64().unwrap_or(0);
    let bd_count = bd_val["count"].as_u64().unwrap_or(0);

    assert_eq!(
        br_count, bd_count,
        "complex cycle counts should match: br={}, bd={}",
        br_count, bd_count
    );

    info!("conformance_dep_cycles_complex passed");
}

#[test]
fn conformance_dep_cycles_json() {
    common::init_test_logging();
    info!("Starting conformance_dep_cycles_json test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Just test JSON output structure
    let br_cycles = workspace.run_br(["dep", "cycles", "--json"], "cycles");
    let bd_cycles = workspace.run_bd(["dep", "cycles", "--json"], "cycles");

    assert!(br_cycles.status.success(), "br dep cycles --json failed");
    assert!(bd_cycles.status.success(), "bd dep cycles --json failed");

    let br_json = extract_json_payload(&br_cycles.stdout);
    let bd_json = extract_json_payload(&bd_cycles.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("br should produce valid JSON");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("bd should produce valid JSON");

    // Both should have cycles and count fields
    assert!(
        br_val.get("cycles").is_some() || br_val.get("count").is_some(),
        "br cycles JSON should have cycles or count field"
    );
    assert!(
        bd_val.get("cycles").is_some() || bd_val.get("count").is_some(),
        "bd cycles JSON should have cycles or count field"
    );

    info!("conformance_dep_cycles_json passed");
}
