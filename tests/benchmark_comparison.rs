//! Benchmark Comparison Tests: br (Rust) vs bd (Go) Performance
//!
//! This module provides comprehensive performance benchmarks comparing the Rust
//! implementation (br) against the Go implementation (bd) for equivalent operations.
//!
//! Run with: cargo test benchmark_ --release -- --nocapture --ignored
//!
//! The benchmarks measure:
//! - Command latency (init, create, list, search, sync)
//! - Throughput (batch creation)
//! - Scaling behavior (list with 10/100/1000 issues)

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

mod common;

use assert_cmd::Command;
use chrono::Utc;
use common::init_test_logging;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tracing::info;

// ============================================================================
// BENCHMARK INFRASTRUCTURE (adapted from conformance.rs)
// ============================================================================

/// Output from running a command
#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub duration: Duration,
}

/// Workspace for benchmark tests with paired br/bd directories
pub struct BenchmarkWorkspace {
    pub temp_dir: TempDir,
    pub br_root: PathBuf,
    pub bd_root: PathBuf,
    pub log_dir: PathBuf,
}

impl BenchmarkWorkspace {
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

    /// Run br command and return duration only (for timing loops)
    pub fn time_br<I, S>(&self, args: I) -> Duration
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let start = Instant::now();
        let _ = self.run_br(args, "timing");
        start.elapsed()
    }

    /// Run bd command and return duration only (for timing loops)
    pub fn time_bd<I, S>(&self, args: I) -> Duration
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let start = Instant::now();
        let _ = self.run_bd(args, "timing");
        start.elapsed()
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
    cmd.env("HOME", cwd);

    let start = Instant::now();
    let output = cmd.output().expect("run br");
    let duration = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Log output
    let log_path = log_dir.join(format!("{label}.log"));
    let log_body = format!(
        "label: {label}\nduration: {:?}\nstatus: {}\n\nstdout:\n{}\n\nstderr:\n{}\n",
        duration, output.status, stdout, stderr
    );
    let _ = fs::write(&log_path, log_body);

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
        success: output.status.success(),
        duration,
    }
}

fn run_bd_cmd<I, S>(cwd: &PathBuf, log_dir: &PathBuf, args: I, label: &str) -> CmdOutput
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = std::process::Command::new("bd");
    cmd.current_dir(cwd);
    cmd.args(args);
    cmd.env("NO_COLOR", "1");
    cmd.env("HOME", cwd);

    let start = Instant::now();
    let output = cmd.output().expect("run bd");
    let duration = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Log output
    let log_path = log_dir.join(format!("{label}.log"));
    let log_body = format!(
        "label: {label}\nduration: {:?}\nstatus: {}\n\nstdout:\n{}\n\nstderr:\n{}\n",
        duration, output.status, stdout, stderr
    );
    let _ = fs::write(&log_path, log_body);

    let entry = RunLogEntry {
        timestamp: Utc::now().to_rfc3339(),
        label: label.to_string(),
        binary: "bd".to_string(),
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
        success: output.status.success(),
        duration,
    }
}

// ============================================================================
// TIMING STATISTICS
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

        let mut ms_values: Vec<f64> = durations.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
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

        let ms_values: Vec<f64> = durations.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        let mean = ms_values.iter().sum::<f64>() / ms_values.len() as f64;
        let variance =
            ms_values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / ms_values.len() as f64;
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

// ============================================================================
// BENCHMARK COMPARISON REPORT
// ============================================================================

/// Result of comparing br vs bd for a single benchmark
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkComparison {
    pub name: String,
    pub description: String,
    pub br_stats: TimingStats,
    pub bd_stats: TimingStats,
    /// Ratio of br time to bd time (< 1 means br is faster)
    pub speedup_ratio: f64,
    /// Percentage speedup (positive = br faster, negative = bd faster)
    pub speedup_percent: f64,
}

impl BenchmarkComparison {
    pub fn new(
        name: &str,
        description: &str,
        br_stats: TimingStats,
        bd_stats: TimingStats,
    ) -> Self {
        let speedup_ratio = if bd_stats.mean_ms > 0.0 {
            br_stats.mean_ms / bd_stats.mean_ms
        } else {
            1.0
        };
        let speedup_percent = if bd_stats.mean_ms > 0.0 {
            ((bd_stats.mean_ms - br_stats.mean_ms) / bd_stats.mean_ms) * 100.0
        } else {
            0.0
        };

        Self {
            name: name.to_string(),
            description: description.to_string(),
            br_stats,
            bd_stats,
            speedup_ratio,
            speedup_percent,
        }
    }

    /// Pretty print the comparison result
    pub fn print(&self) {
        println!("\n=== {} ===", self.name);
        println!("Description: {}", self.description);
        println!(
            "br: mean={:.2}ms median={:.2}ms p95={:.2}ms",
            self.br_stats.mean_ms, self.br_stats.median_ms, self.br_stats.p95_ms
        );
        println!(
            "bd: mean={:.2}ms median={:.2}ms p95={:.2}ms",
            self.bd_stats.mean_ms, self.bd_stats.median_ms, self.bd_stats.p95_ms
        );

        if self.speedup_percent > 0.0 {
            println!(
                "Result: br is {:.1}% FASTER (ratio: {:.2}x)",
                self.speedup_percent, self.speedup_ratio
            );
        } else if self.speedup_percent < 0.0 {
            println!(
                "Result: br is {:.1}% SLOWER (ratio: {:.2}x)",
                -self.speedup_percent, self.speedup_ratio
            );
        } else {
            println!(
                "Result: Similar performance (ratio: {:.2}x)",
                self.speedup_ratio
            );
        }
    }
}

/// Full benchmark report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub timestamp: String,
    pub config: BenchmarkConfigJson,
    pub comparisons: Vec<BenchmarkComparison>,
    pub summary: BenchmarkSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkConfigJson {
    pub warmup_runs: usize,
    pub timed_runs: usize,
    pub outlier_threshold: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkSummary {
    pub total_benchmarks: usize,
    pub br_faster_count: usize,
    pub bd_faster_count: usize,
    pub avg_speedup_percent: f64,
    pub avg_speedup_ratio: f64,
}

impl BenchmarkReport {
    pub fn new(config: &BenchmarkConfig, comparisons: Vec<BenchmarkComparison>) -> Self {
        let total = comparisons.len();
        let br_faster = comparisons
            .iter()
            .filter(|c| c.speedup_percent > 0.0)
            .count();
        let bd_faster = comparisons
            .iter()
            .filter(|c| c.speedup_percent < 0.0)
            .count();

        let avg_speedup = if total > 0 {
            comparisons.iter().map(|c| c.speedup_percent).sum::<f64>() / total as f64
        } else {
            0.0
        };

        let avg_ratio = if total > 0 {
            comparisons.iter().map(|c| c.speedup_ratio).sum::<f64>() / total as f64
        } else {
            1.0
        };

        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            config: BenchmarkConfigJson {
                warmup_runs: config.warmup_runs,
                timed_runs: config.timed_runs,
                outlier_threshold: config.outlier_threshold,
            },
            comparisons,
            summary: BenchmarkSummary {
                total_benchmarks: total,
                br_faster_count: br_faster,
                bd_faster_count: bd_faster,
                avg_speedup_percent: avg_speedup,
                avg_speedup_ratio: avg_ratio,
            },
        }
    }

    /// Print summary report
    pub fn print_summary(&self) {
        println!("\n========================================");
        println!("BENCHMARK COMPARISON REPORT");
        println!("========================================");
        println!("Timestamp: {}", self.timestamp);
        println!(
            "Config: {} warmup, {} timed runs, {:.1}x outlier threshold",
            self.config.warmup_runs, self.config.timed_runs, self.config.outlier_threshold
        );
        println!("");

        for comparison in &self.comparisons {
            comparison.print();
        }

        println!("\n========================================");
        println!("SUMMARY");
        println!("========================================");
        println!("Total benchmarks: {}", self.summary.total_benchmarks);
        println!(
            "br faster: {} ({:.0}%)",
            self.summary.br_faster_count,
            100.0 * self.summary.br_faster_count as f64 / self.summary.total_benchmarks as f64
        );
        println!(
            "bd faster: {} ({:.0}%)",
            self.summary.bd_faster_count,
            100.0 * self.summary.bd_faster_count as f64 / self.summary.total_benchmarks as f64
        );
        println!(
            "Average speedup: {:.1}% ({:.2}x ratio)",
            self.summary.avg_speedup_percent, self.summary.avg_speedup_ratio
        );

        if self.summary.avg_speedup_percent > 0.0 {
            println!("\nOverall: br (Rust) is faster on average");
        } else if self.summary.avg_speedup_percent < 0.0 {
            println!("\nOverall: bd (Go) is faster on average");
        } else {
            println!("\nOverall: Similar performance");
        }
    }

    /// Export to JSON
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

// ============================================================================
// BENCHMARK TESTS
// ============================================================================

/// Benchmark: init command (cold start)
fn benchmark_init(config: &BenchmarkConfig) -> BenchmarkComparison {
    info!("benchmark_init: starting");

    let br_stats = run_benchmark(config, || {
        let workspace = BenchmarkWorkspace::new();
        workspace.time_br(["init"])
    });

    let bd_stats = run_benchmark(config, || {
        let workspace = BenchmarkWorkspace::new();
        workspace.time_bd(["init"])
    });

    info!(
        "benchmark_init: br_mean={:.2}ms bd_mean={:.2}ms",
        br_stats.mean_ms, bd_stats.mean_ms
    );

    BenchmarkComparison::new(
        "init",
        "Initialize workspace (cold start)",
        br_stats,
        bd_stats,
    )
}

/// Benchmark: create single issue
fn benchmark_create_single(config: &BenchmarkConfig) -> BenchmarkComparison {
    info!("benchmark_create_single: starting");

    // Setup workspace once, then time create operations
    let workspace = BenchmarkWorkspace::new();
    workspace.init_both();

    let mut br_counter = 0;
    let br_stats = run_benchmark(config, || {
        let title = format!("Benchmark issue {}", br_counter);
        br_counter += 1;
        workspace.time_br(["create", &title, "--json"])
    });

    let mut bd_counter = 0;
    let bd_stats = run_benchmark(config, || {
        let title = format!("Benchmark issue {}", bd_counter);
        bd_counter += 1;
        workspace.time_bd(["create", &title, "--json"])
    });

    info!(
        "benchmark_create_single: br_mean={:.2}ms bd_mean={:.2}ms",
        br_stats.mean_ms, bd_stats.mean_ms
    );

    BenchmarkComparison::new("create_single", "Create single issue", br_stats, bd_stats)
}

/// Benchmark: create 100 issues (throughput)
fn benchmark_create_batch_100(config: &BenchmarkConfig) -> BenchmarkComparison {
    info!("benchmark_create_batch_100: starting");

    let br_stats = run_benchmark(config, || {
        let workspace = BenchmarkWorkspace::new();
        let _ = workspace.run_br(["init"], "init");

        let start = Instant::now();
        for i in 0..100 {
            let title = format!("Batch issue {}", i);
            let _ = workspace.run_br(["create", &title, "--json"], "create");
        }
        start.elapsed()
    });

    let bd_stats = run_benchmark(config, || {
        let workspace = BenchmarkWorkspace::new();
        let _ = workspace.run_bd(["init"], "init");

        let start = Instant::now();
        for i in 0..100 {
            let title = format!("Batch issue {}", i);
            let _ = workspace.run_bd(["create", &title, "--json"], "create");
        }
        start.elapsed()
    });

    info!(
        "benchmark_create_batch_100: br_mean={:.2}ms bd_mean={:.2}ms",
        br_stats.mean_ms, bd_stats.mean_ms
    );

    BenchmarkComparison::new(
        "create_batch_100",
        "Create 100 issues (throughput)",
        br_stats,
        bd_stats,
    )
}

/// Helper to populate workspace with N issues
fn populate_workspace(workspace: &BenchmarkWorkspace, count: usize) {
    workspace.init_both();

    for i in 0..count {
        let title = format!("Issue {}", i);
        let priority = format!("{}", i % 5);
        workspace.run_br(
            ["create", &title, "--priority", &priority, "--json"],
            "setup",
        );
        workspace.run_bd(
            ["create", &title, "--priority", &priority, "--json"],
            "setup",
        );
    }
}

/// Benchmark: list with 10 issues
fn benchmark_list_10(config: &BenchmarkConfig) -> BenchmarkComparison {
    info!("benchmark_list_10: starting");

    let workspace = BenchmarkWorkspace::new();
    populate_workspace(&workspace, 10);

    let br_stats = run_benchmark(config, || workspace.time_br(["list", "--json"]));

    let bd_stats = run_benchmark(config, || workspace.time_bd(["list", "--json"]));

    info!(
        "benchmark_list_10: br_mean={:.2}ms bd_mean={:.2}ms",
        br_stats.mean_ms, bd_stats.mean_ms
    );

    BenchmarkComparison::new("list_10", "List 10 issues", br_stats, bd_stats)
}

/// Benchmark: list with 100 issues
fn benchmark_list_100(config: &BenchmarkConfig) -> BenchmarkComparison {
    info!("benchmark_list_100: starting");

    let workspace = BenchmarkWorkspace::new();
    populate_workspace(&workspace, 100);

    let br_stats = run_benchmark(config, || workspace.time_br(["list", "--json"]));

    let bd_stats = run_benchmark(config, || workspace.time_bd(["list", "--json"]));

    info!(
        "benchmark_list_100: br_mean={:.2}ms bd_mean={:.2}ms",
        br_stats.mean_ms, bd_stats.mean_ms
    );

    BenchmarkComparison::new("list_100", "List 100 issues", br_stats, bd_stats)
}

/// Benchmark: list with status filter
fn benchmark_list_filtered(config: &BenchmarkConfig) -> BenchmarkComparison {
    info!("benchmark_list_filtered: starting");

    let workspace = BenchmarkWorkspace::new();
    populate_workspace(&workspace, 50);

    let br_stats = run_benchmark(config, || {
        workspace.time_br(["list", "--status=open", "--json"])
    });

    let bd_stats = run_benchmark(config, || {
        workspace.time_bd(["list", "--status=open", "--json"])
    });

    info!(
        "benchmark_list_filtered: br_mean={:.2}ms bd_mean={:.2}ms",
        br_stats.mean_ms, bd_stats.mean_ms
    );

    BenchmarkComparison::new(
        "list_filtered",
        "List with status filter (50 issues)",
        br_stats,
        bd_stats,
    )
}

/// Benchmark: search command
fn benchmark_search(config: &BenchmarkConfig) -> BenchmarkComparison {
    info!("benchmark_search: starting");

    let workspace = BenchmarkWorkspace::new();
    populate_workspace(&workspace, 50);

    let br_stats = run_benchmark(config, || workspace.time_br(["search", "Issue", "--json"]));

    let bd_stats = run_benchmark(config, || workspace.time_bd(["search", "Issue", "--json"]));

    info!(
        "benchmark_search: br_mean={:.2}ms bd_mean={:.2}ms",
        br_stats.mean_ms, bd_stats.mean_ms
    );

    BenchmarkComparison::new("search", "Full-text search (50 issues)", br_stats, bd_stats)
}

/// Benchmark: ready command (unblocked issues)
fn benchmark_ready(config: &BenchmarkConfig) -> BenchmarkComparison {
    info!("benchmark_ready: starting");

    let workspace = BenchmarkWorkspace::new();
    populate_workspace(&workspace, 30);

    let br_stats = run_benchmark(config, || workspace.time_br(["ready", "--json"]));

    let bd_stats = run_benchmark(config, || workspace.time_bd(["ready", "--json"]));

    info!(
        "benchmark_ready: br_mean={:.2}ms bd_mean={:.2}ms",
        br_stats.mean_ms, bd_stats.mean_ms
    );

    BenchmarkComparison::new("ready", "Get ready issues (30 issues)", br_stats, bd_stats)
}

/// Benchmark: sync --flush-only (export to JSONL)
fn benchmark_sync_flush(config: &BenchmarkConfig) -> BenchmarkComparison {
    info!("benchmark_sync_flush: starting");

    let workspace = BenchmarkWorkspace::new();
    populate_workspace(&workspace, 50);

    let br_stats = run_benchmark(config, || workspace.time_br(["sync", "--flush-only"]));

    let bd_stats = run_benchmark(config, || workspace.time_bd(["sync", "--flush-only"]));

    info!(
        "benchmark_sync_flush: br_mean={:.2}ms bd_mean={:.2}ms",
        br_stats.mean_ms, bd_stats.mean_ms
    );

    BenchmarkComparison::new(
        "sync_flush",
        "Sync flush to JSONL (50 issues)",
        br_stats,
        bd_stats,
    )
}

/// Benchmark: stats command
fn benchmark_stats(config: &BenchmarkConfig) -> BenchmarkComparison {
    info!("benchmark_stats: starting");

    let workspace = BenchmarkWorkspace::new();
    populate_workspace(&workspace, 30);

    let br_stats = run_benchmark(config, || workspace.time_br(["stats", "--json"]));

    let bd_stats = run_benchmark(config, || workspace.time_bd(["stats", "--json"]));

    info!(
        "benchmark_stats: br_mean={:.2}ms bd_mean={:.2}ms",
        br_stats.mean_ms, bd_stats.mean_ms
    );

    BenchmarkComparison::new("stats", "Get project stats (30 issues)", br_stats, bd_stats)
}

// ============================================================================
// MAIN BENCHMARK TEST
// ============================================================================

/// Run all benchmarks and generate report
///
/// This test is marked as #[ignore] because it takes several minutes to run.
/// Execute with: cargo test benchmark_comparison_full --release -- --ignored --nocapture
#[test]
#[ignore]
fn benchmark_comparison_full() {
    init_test_logging();

    println!("\n");
    println!("========================================");
    println!("STARTING BR VS BD BENCHMARK COMPARISON");
    println!("========================================");
    println!("This will take several minutes...\n");

    let config = BenchmarkConfig {
        warmup_runs: 2,
        timed_runs: 5,
        outlier_threshold: 2.0,
    };

    let mut comparisons = Vec::new();

    // Command latency benchmarks
    println!("[1/10] Running init benchmark...");
    comparisons.push(benchmark_init(&config));

    println!("[2/10] Running create_single benchmark...");
    comparisons.push(benchmark_create_single(&config));

    println!("[3/10] Running create_batch_100 benchmark (this takes a while)...");
    comparisons.push(benchmark_create_batch_100(&config));

    println!("[4/10] Running list_10 benchmark...");
    comparisons.push(benchmark_list_10(&config));

    println!("[5/10] Running list_100 benchmark...");
    comparisons.push(benchmark_list_100(&config));

    println!("[6/10] Running list_filtered benchmark...");
    comparisons.push(benchmark_list_filtered(&config));

    println!("[7/10] Running search benchmark...");
    comparisons.push(benchmark_search(&config));

    println!("[8/10] Running ready benchmark...");
    comparisons.push(benchmark_ready(&config));

    println!("[9/10] Running sync_flush benchmark...");
    comparisons.push(benchmark_sync_flush(&config));

    println!("[10/10] Running stats benchmark...");
    comparisons.push(benchmark_stats(&config));

    // Generate report
    let report = BenchmarkReport::new(&config, comparisons);

    // Print summary
    report.print_summary();

    // Save JSON report
    let json_report = report.to_json();
    println!("\n========================================");
    println!("JSON REPORT");
    println!("========================================");
    println!("{}", json_report);

    // Optionally save to file
    let report_path = std::env::temp_dir().join("br_bd_benchmark_report.json");
    if let Err(e) = fs::write(&report_path, &json_report) {
        eprintln!(
            "Warning: Could not save report to {}: {}",
            report_path.display(),
            e
        );
    } else {
        println!("\nReport saved to: {}", report_path.display());
    }
}

/// Quick benchmark test (fewer runs, smaller datasets) for CI
#[test]
fn benchmark_comparison_quick() {
    init_test_logging();

    info!("benchmark_comparison_quick: starting");

    let config = BenchmarkConfig {
        warmup_runs: 1,
        timed_runs: 3,
        outlier_threshold: 3.0,
    };

    // Just run init and create_single as quick sanity check
    let init_result = benchmark_init(&config);
    assert!(
        init_result.br_stats.mean_ms > 0.0,
        "br init should have positive timing"
    );
    assert!(
        init_result.bd_stats.mean_ms > 0.0,
        "bd init should have positive timing"
    );

    init_result.print();

    let create_result = benchmark_create_single(&config);
    assert!(
        create_result.br_stats.mean_ms > 0.0,
        "br create should have positive timing"
    );
    assert!(
        create_result.bd_stats.mean_ms > 0.0,
        "bd create should have positive timing"
    );

    create_result.print();

    info!("benchmark_comparison_quick: completed successfully");
}

/// Test that benchmark infrastructure works correctly
#[test]
fn benchmark_infrastructure_works() {
    init_test_logging();

    info!("benchmark_infrastructure_works: testing BenchmarkWorkspace");

    // Test workspace creation
    let workspace = BenchmarkWorkspace::new();
    assert!(workspace.br_root.exists());
    assert!(workspace.bd_root.exists());

    // Test init
    let (br_out, bd_out) = workspace.init_both();
    assert!(br_out.success, "br init failed: {}", br_out.stderr);
    assert!(bd_out.success, "bd init failed: {}", bd_out.stderr);

    // Test create
    let br_create = workspace.run_br(["create", "Test issue", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Test issue", "--json"], "create");
    assert!(br_create.success, "br create failed: {}", br_create.stderr);
    assert!(bd_create.success, "bd create failed: {}", bd_create.stderr);

    // Test timing functions
    let br_duration = workspace.time_br(["list", "--json"]);
    let bd_duration = workspace.time_bd(["list", "--json"]);
    assert!(br_duration.as_millis() > 0, "br timing should be positive");
    assert!(bd_duration.as_millis() > 0, "bd timing should be positive");

    info!("benchmark_infrastructure_works: all checks passed");
}

/// Test TimingStats calculations
#[test]
fn test_timing_stats_calculations() {
    init_test_logging();

    let durations = vec![
        Duration::from_millis(10),
        Duration::from_millis(12),
        Duration::from_millis(11),
        Duration::from_millis(13),
        Duration::from_millis(11),
    ];

    let stats = TimingStats::from_durations(&durations);

    // Mean should be ~11.4ms
    assert!(
        (stats.mean_ms - 11.4).abs() < 0.1,
        "mean_ms was {}",
        stats.mean_ms
    );

    // Median should be 11ms
    assert!(
        (stats.median_ms - 11.0).abs() < 0.1,
        "median_ms was {}",
        stats.median_ms
    );

    // Min should be 10ms
    assert!(
        (stats.min_ms - 10.0).abs() < 0.1,
        "min_ms was {}",
        stats.min_ms
    );

    // Max should be 13ms
    assert!(
        (stats.max_ms - 13.0).abs() < 0.1,
        "max_ms was {}",
        stats.max_ms
    );

    assert_eq!(stats.run_count, 5);

    info!("test_timing_stats_calculations: passed");
}

/// Test outlier filtering
#[test]
fn test_outlier_filtering() {
    init_test_logging();

    let durations = vec![
        Duration::from_millis(10),
        Duration::from_millis(11),
        Duration::from_millis(12),
        Duration::from_millis(11),
        Duration::from_millis(10),
        Duration::from_millis(11),
        Duration::from_millis(500), // Outlier - needs to be extreme enough with this sample size
    ];

    let filtered = TimingStats::filter_outliers(&durations, 2.0);

    // Should filter out the 500ms outlier
    assert!(
        filtered.len() < durations.len(),
        "Should have filtered an outlier"
    );

    let stats = TimingStats::from_durations(&filtered);
    assert!(
        stats.max_ms < 100.0,
        "Outlier should be removed, max was {}",
        stats.max_ms
    );

    info!("test_outlier_filtering: passed");
}

/// Test BenchmarkComparison calculations
#[test]
fn test_benchmark_comparison_calculations() {
    init_test_logging();

    let br_stats = TimingStats {
        mean_ms: 10.0,
        median_ms: 10.0,
        p95_ms: 12.0,
        std_dev_ms: 1.0,
        min_ms: 9.0,
        max_ms: 12.0,
        run_count: 5,
    };

    let bd_stats = TimingStats {
        mean_ms: 20.0,
        median_ms: 20.0,
        p95_ms: 22.0,
        std_dev_ms: 1.0,
        min_ms: 19.0,
        max_ms: 22.0,
        run_count: 5,
    };

    let comparison = BenchmarkComparison::new("test", "Test benchmark", br_stats, bd_stats);

    // br is 2x faster (10ms vs 20ms), so ratio should be 0.5
    assert!(
        (comparison.speedup_ratio - 0.5).abs() < 0.01,
        "Speedup ratio should be 0.5, was {}",
        comparison.speedup_ratio
    );

    // Speedup percent should be 50% (br is 50% faster)
    assert!(
        (comparison.speedup_percent - 50.0).abs() < 0.1,
        "Speedup percent should be 50%, was {}",
        comparison.speedup_percent
    );

    info!("test_benchmark_comparison_calculations: passed");
}
