//! Real Dataset Benchmarks: br (Rust) vs bd (Go) Performance on Real Data
//!
//! This module benchmarks both implementations on actual beads datasets from
//! real projects (beads_rust, beads_viewer, coding_agent_session_search, brenner_bot).
//!
//! Run with: cargo test benchmark_dataset --release -- --nocapture --ignored
//!
//! Measures:
//! - Read-heavy workloads (list, search, ready, stats)
//! - Write-heavy workloads (create, update, close)
//! - Time + RSS for br and bd
//! - Per-dataset comparison tables

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

mod common;

use common::{
    BaselineStore, DatasetIntegrityGuard, DatasetMetadata, DatasetRegistry, KnownDataset,
    RegressionConfig, RegressionResult, RegressionSummary, init_test_logging,
    should_update_baseline, update_baselines_from_results,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::info;

// ============================================================================
// BENCHMARK INFRASTRUCTURE
// ============================================================================

/// Check if the `bd` (Go beads) binary is available on the system.
/// Returns true if `bd version` runs successfully, false otherwise.
fn bd_available() -> bool {
    std::process::Command::new("bd")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Skip test if bd binary is not available (used in CI where only br is built)
macro_rules! skip_if_no_bd {
    () => {
        if !bd_available() {
            eprintln!("Skipping test: 'bd' binary not found (expected in CI)");
            return;
        }
    };
}

/// Output from running a command
#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub duration: Duration,
    pub exit_code: i32,
}

/// Workspace for dataset benchmarks - uses separate directories for br and bd
/// to avoid schema compatibility issues between implementations
pub struct DatasetBenchmarkWorkspace {
    pub temp_dir: tempfile::TempDir,
    pub br_root: PathBuf,
    pub bd_root: PathBuf,
    pub log_dir: PathBuf,
    pub metadata: DatasetMetadata,
}

impl DatasetBenchmarkWorkspace {
    /// Create from a known dataset (copies to two separate directories)
    pub fn from_dataset(dataset: KnownDataset) -> std::io::Result<Self> {
        let source_beads = dataset.beads_dir();
        if !source_beads.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Dataset {} not found", dataset.name()),
            ));
        }

        let temp_dir = tempfile::TempDir::new()?;
        let root = temp_dir.path();

        let br_root = root.join("br_workspace");
        let bd_root = root.join("bd_workspace");
        let log_dir = root.join("benchmark-logs");

        fs::create_dir_all(&br_root)?;
        fs::create_dir_all(&bd_root)?;
        fs::create_dir_all(&log_dir)?;

        // Copy .beads to both workspaces
        copy_beads_dir(&source_beads, &br_root.join(".beads"))?;
        copy_beads_dir(&source_beads, &bd_root.join(".beads"))?;

        // Create minimal git scaffold in both
        for workspace in [&br_root, &bd_root] {
            fs::create_dir_all(workspace.join(".git"))?;
            fs::write(
                workspace.join(".git").join("HEAD"),
                "ref: refs/heads/main\n",
            )?;
        }

        // Compute metadata from one of the copies
        let jsonl_path = br_root.join(".beads").join("issues.jsonl");
        let db_path = br_root.join(".beads").join("beads.db");
        let jsonl_size_bytes = fs::metadata(&jsonl_path).map(|m| m.len()).unwrap_or(0);
        let db_size_bytes = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
        let issue_count = count_jsonl_lines(&jsonl_path);

        let metadata = DatasetMetadata {
            name: dataset.name().to_string(),
            source_path: dataset.source_path(),
            issue_count,
            jsonl_size_bytes,
            db_size_bytes,
            dependency_count: 0,
            content_hash: "benchmark".to_string(),
            copied_at: Some(std::time::SystemTime::now()),
            copy_duration: None,
            source_commit: None,
            is_override: false,
            override_reason: None,
        };

        Ok(Self {
            temp_dir,
            br_root,
            bd_root,
            log_dir,
            metadata,
        })
    }

    /// Create empty workspaces for init benchmarks
    pub fn empty() -> std::io::Result<Self> {
        let temp_dir = tempfile::TempDir::new()?;
        let root = temp_dir.path();

        let br_root = root.join("br_workspace");
        let bd_root = root.join("bd_workspace");
        let log_dir = root.join("benchmark-logs");

        fs::create_dir_all(&br_root)?;
        fs::create_dir_all(&bd_root)?;
        fs::create_dir_all(&log_dir)?;

        // Create minimal git scaffold in both
        for workspace in [&br_root, &bd_root] {
            fs::create_dir_all(workspace.join(".git"))?;
            fs::write(
                workspace.join(".git").join("HEAD"),
                "ref: refs/heads/main\n",
            )?;
        }

        let metadata = DatasetMetadata {
            name: "empty".to_string(),
            source_path: PathBuf::new(),
            issue_count: 0,
            jsonl_size_bytes: 0,
            db_size_bytes: 0,
            dependency_count: 0,
            content_hash: "empty".to_string(),
            copied_at: Some(std::time::SystemTime::now()),
            copy_duration: None,
            source_commit: None,
            is_override: false,
            override_reason: None,
        };

        Ok(Self {
            temp_dir,
            br_root,
            bd_root,
            log_dir,
            metadata,
        })
    }

    /// Get metadata
    pub fn metadata(&self) -> &DatasetMetadata {
        &self.metadata
    }

    /// Run br command
    pub fn run_br<I, S>(&self, args: I) -> CmdOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run_cmd("br", &self.br_root, args)
    }

    /// Run bd command
    pub fn run_bd<I, S>(&self, args: I) -> CmdOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run_cmd("bd", &self.bd_root, args)
    }

    /// Time br command (returns just duration)
    pub fn time_br<I, S>(&self, args: I) -> Duration
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_br(args).duration
    }

    /// Time bd command (returns just duration)
    pub fn time_bd<I, S>(&self, args: I) -> Duration
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_bd(args).duration
    }
}

/// Copy .beads directory excluding temp files
fn copy_beads_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);

        let name = file_name.to_string_lossy();

        // Skip socket files, WAL/SHM, sync lock, and history
        if name.ends_with(".sock")
            || name.ends_with("-wal")
            || name.ends_with("-shm")
            || name == ".sync.lock"
            || name == "history"
        {
            continue;
        }

        if file_type.is_dir() {
            copy_beads_dir(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

/// Count lines in JSONL file
fn count_jsonl_lines(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

fn run_cmd<I, S>(binary: &str, cwd: &Path, args: I) -> CmdOutput
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let start = Instant::now();

    let output = if binary == "br" {
        let br_bin = assert_cmd::cargo::cargo_bin!("br");
        std::process::Command::new(&br_bin)
            .current_dir(cwd)
            .args(args)
            .env("NO_COLOR", "1")
            .output()
            .expect("run br")
    } else {
        std::process::Command::new(binary)
            .current_dir(cwd)
            .args(args)
            .env("NO_COLOR", "1")
            .env("HOME", cwd)
            .output()
            .expect("run bd")
    };

    let duration = start.elapsed();

    CmdOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        success: output.status.success(),
        duration,
        exit_code: output.status.code().unwrap_or(-1),
    }
}

// ============================================================================
// TIMING STATISTICS
// ============================================================================

/// Timing statistics from benchmark runs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimingStats {
    pub mean_ms: f64,
    pub median_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub run_count: usize,
}

impl TimingStats {
    pub fn from_durations(durations: &[Duration]) -> Self {
        if durations.is_empty() {
            return Self {
                mean_ms: 0.0,
                median_ms: 0.0,
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

        Self {
            mean_ms: mean,
            median_ms: median,
            min_ms: ms_values[0],
            max_ms: ms_values[n - 1],
            run_count: n,
        }
    }
}

// ============================================================================
// MEMORY MEASUREMENT
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStats {
    pub max_rss_kb: Option<u64>,
}

fn parse_max_rss_kb(stderr: &str) -> Option<u64> {
    for line in stderr.lines() {
        if let Some(rest) = line.strip_prefix("Maximum resident set size (kbytes):") {
            return rest.trim().parse::<u64>().ok();
        }
    }
    None
}

fn measure_rss(binary: &str, cwd: &Path, args: &[&str]) -> MemoryStats {
    let time_path = Path::new("/usr/bin/time");
    if !time_path.exists() {
        return MemoryStats { max_rss_kb: None };
    }

    let program = if binary == "br" {
        assert_cmd::cargo::cargo_bin!("br").to_path_buf()
    } else {
        PathBuf::from("bd")
    };

    let output = std::process::Command::new(time_path)
        .arg("-v")
        .arg(program)
        .args(args)
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .env("HOME", cwd)
        .output()
        .expect("run /usr/bin/time");

    let stderr = String::from_utf8_lossy(&output.stderr);
    MemoryStats {
        max_rss_kb: parse_max_rss_kb(&stderr),
    }
}

// ============================================================================
// BENCHMARK COMPARISON RESULT
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadResult {
    pub name: String,
    pub br_stats: TimingStats,
    pub bd_stats: TimingStats,
    pub br_rss_kb: Option<u64>,
    pub bd_rss_kb: Option<u64>,
    pub speedup_percent: f64,
}

impl WorkloadResult {
    pub fn new(
        name: &str,
        br_stats: TimingStats,
        bd_stats: TimingStats,
        br_rss: MemoryStats,
        bd_rss: MemoryStats,
    ) -> Self {
        let speedup_percent = if br_stats.mean_ms > 0.0 {
            ((bd_stats.mean_ms - br_stats.mean_ms) / bd_stats.mean_ms) * 100.0
        } else {
            0.0
        };

        Self {
            name: name.to_string(),
            br_stats,
            bd_stats,
            br_rss_kb: br_rss.max_rss_kb,
            bd_rss_kb: bd_rss.max_rss_kb,
            speedup_percent,
        }
    }

    pub fn print(&self) {
        let winner = if self.speedup_percent > 5.0 {
            "br"
        } else if self.speedup_percent < -5.0 {
            "bd"
        } else {
            "tie"
        };

        println!(
            "  {:<20} br: {:>8.1}ms  bd: {:>8.1}ms  ({:>+6.1}% {})",
            self.name, self.br_stats.mean_ms, self.bd_stats.mean_ms, self.speedup_percent, winner
        );

        if let (Some(br_rss), Some(bd_rss)) = (self.br_rss_kb, self.bd_rss_kb) {
            println!(
                "                       RSS: br {:>6}KB  bd {:>6}KB",
                br_rss, bd_rss
            );
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetBenchmarkResult {
    pub dataset_name: String,
    pub issue_count: usize,
    pub db_size_bytes: u64,
    pub read_workloads: Vec<WorkloadResult>,
    pub write_workloads: Vec<WorkloadResult>,
}

impl DatasetBenchmarkResult {
    pub fn print_table(&self) {
        println!("\n══════════════════════════════════════════════════════════════");
        println!(
            "Dataset: {} ({} issues, {} bytes DB)",
            self.dataset_name, self.issue_count, self.db_size_bytes
        );
        println!("══════════════════════════════════════════════════════════════");

        if !self.read_workloads.is_empty() {
            println!("\n  READ-HEAVY WORKLOADS:");
            for result in &self.read_workloads {
                result.print();
            }
        }

        if !self.write_workloads.is_empty() {
            println!("\n  WRITE-HEAVY WORKLOADS:");
            for result in &self.write_workloads {
                result.print();
            }
        }
    }
}

// ============================================================================
// BENCHMARK CONFIGURATION
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchConfig {
    pub warmup_runs: usize,
    pub timed_runs: usize,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            warmup_runs: 2,
            timed_runs: 5,
        }
    }
}

// ============================================================================
// READ-HEAVY WORKLOAD BENCHMARKS
// ============================================================================

fn benchmark_list(workspace: &DatasetBenchmarkWorkspace, config: &BenchConfig) -> WorkloadResult {
    let args = ["list", "--json"];

    // Warmup
    for _ in 0..config.warmup_runs {
        let _ = workspace.run_br(&args);
        let _ = workspace.run_bd(&args);
    }

    // Timed runs
    let br_durations: Vec<Duration> = (0..config.timed_runs)
        .map(|_| workspace.time_br(&args))
        .collect();
    let bd_durations: Vec<Duration> = (0..config.timed_runs)
        .map(|_| workspace.time_bd(&args))
        .collect();

    // Memory measurement
    let br_rss = measure_rss("br", &workspace.br_root, &args);
    let bd_rss = measure_rss("bd", &workspace.bd_root, &args);

    WorkloadResult::new(
        "list --json",
        TimingStats::from_durations(&br_durations),
        TimingStats::from_durations(&bd_durations),
        br_rss,
        bd_rss,
    )
}

fn benchmark_search(workspace: &DatasetBenchmarkWorkspace, config: &BenchConfig) -> WorkloadResult {
    let args = ["search", "test", "--json"];

    // Warmup
    for _ in 0..config.warmup_runs {
        let _ = workspace.run_br(&args);
        let _ = workspace.run_bd(&args);
    }

    // Timed runs
    let br_durations: Vec<Duration> = (0..config.timed_runs)
        .map(|_| workspace.time_br(&args))
        .collect();
    let bd_durations: Vec<Duration> = (0..config.timed_runs)
        .map(|_| workspace.time_bd(&args))
        .collect();

    let br_rss = measure_rss("br", &workspace.br_root, &args);
    let bd_rss = measure_rss("bd", &workspace.bd_root, &args);

    WorkloadResult::new(
        "search 'test'",
        TimingStats::from_durations(&br_durations),
        TimingStats::from_durations(&bd_durations),
        br_rss,
        bd_rss,
    )
}

fn benchmark_ready(workspace: &DatasetBenchmarkWorkspace, config: &BenchConfig) -> WorkloadResult {
    let args = ["ready", "--json"];

    // Warmup
    for _ in 0..config.warmup_runs {
        let _ = workspace.run_br(&args);
        let _ = workspace.run_bd(&args);
    }

    // Timed runs
    let br_durations: Vec<Duration> = (0..config.timed_runs)
        .map(|_| workspace.time_br(&args))
        .collect();
    let bd_durations: Vec<Duration> = (0..config.timed_runs)
        .map(|_| workspace.time_bd(&args))
        .collect();

    let br_rss = measure_rss("br", &workspace.br_root, &args);
    let bd_rss = measure_rss("bd", &workspace.bd_root, &args);

    WorkloadResult::new(
        "ready --json",
        TimingStats::from_durations(&br_durations),
        TimingStats::from_durations(&bd_durations),
        br_rss,
        bd_rss,
    )
}

fn benchmark_stats(workspace: &DatasetBenchmarkWorkspace, config: &BenchConfig) -> WorkloadResult {
    let args = ["stats", "--json"];

    // Warmup
    for _ in 0..config.warmup_runs {
        let _ = workspace.run_br(&args);
        let _ = workspace.run_bd(&args);
    }

    // Timed runs
    let br_durations: Vec<Duration> = (0..config.timed_runs)
        .map(|_| workspace.time_br(&args))
        .collect();
    let bd_durations: Vec<Duration> = (0..config.timed_runs)
        .map(|_| workspace.time_bd(&args))
        .collect();

    let br_rss = measure_rss("br", &workspace.br_root, &args);
    let bd_rss = measure_rss("bd", &workspace.bd_root, &args);

    WorkloadResult::new(
        "stats --json",
        TimingStats::from_durations(&br_durations),
        TimingStats::from_durations(&bd_durations),
        br_rss,
        bd_rss,
    )
}

fn benchmark_blocked(
    workspace: &DatasetBenchmarkWorkspace,
    config: &BenchConfig,
) -> WorkloadResult {
    let args = ["blocked", "--json"];

    // Warmup
    for _ in 0..config.warmup_runs {
        let _ = workspace.run_br(&args);
        let _ = workspace.run_bd(&args);
    }

    // Timed runs
    let br_durations: Vec<Duration> = (0..config.timed_runs)
        .map(|_| workspace.time_br(&args))
        .collect();
    let bd_durations: Vec<Duration> = (0..config.timed_runs)
        .map(|_| workspace.time_bd(&args))
        .collect();

    let br_rss = measure_rss("br", &workspace.br_root, &args);
    let bd_rss = measure_rss("bd", &workspace.bd_root, &args);

    WorkloadResult::new(
        "blocked --json",
        TimingStats::from_durations(&br_durations),
        TimingStats::from_durations(&bd_durations),
        br_rss,
        bd_rss,
    )
}

// ============================================================================
// WRITE-HEAVY WORKLOAD BENCHMARKS
// ============================================================================

fn benchmark_create(workspace: &DatasetBenchmarkWorkspace, config: &BenchConfig) -> WorkloadResult {
    // Create issues with unique titles to avoid conflicts
    let mut br_durations = Vec::with_capacity(config.timed_runs);
    let mut bd_durations = Vec::with_capacity(config.timed_runs);

    // Warmup
    for i in 0..config.warmup_runs {
        let title = format!("Warmup issue {i}");
        let _ = workspace.run_br(["create", &title, "--type", "task"]);
        let _ = workspace.run_bd(["create", &title, "--type", "task"]);
    }

    // Timed runs
    for i in 0..config.timed_runs {
        let br_title = format!("BR benchmark issue {i}");
        let bd_title = format!("BD benchmark issue {i}");

        let br_start = Instant::now();
        let _ = workspace.run_br(["create", &br_title, "--type", "task"]);
        br_durations.push(br_start.elapsed());

        let bd_start = Instant::now();
        let _ = workspace.run_bd(["create", &bd_title, "--type", "task"]);
        bd_durations.push(bd_start.elapsed());
    }

    // RSS measurement for create
    let br_rss = measure_rss(
        "br",
        &workspace.br_root,
        &["create", "RSS test br", "--type", "task"],
    );
    let bd_rss = measure_rss(
        "bd",
        &workspace.bd_root,
        &["create", "RSS test bd", "--type", "task"],
    );

    WorkloadResult::new(
        "create",
        TimingStats::from_durations(&br_durations),
        TimingStats::from_durations(&bd_durations),
        br_rss,
        bd_rss,
    )
}

fn benchmark_update(workspace: &DatasetBenchmarkWorkspace, config: &BenchConfig) -> WorkloadResult {
    // First, get list of issues to update
    let br_output = workspace.run_br(["list", "--json"]);
    let issues: Vec<serde_json::Value> =
        serde_json::from_str(&br_output.stdout).unwrap_or_default();

    if issues.is_empty() {
        return WorkloadResult::new(
            "update",
            TimingStats::from_durations(&[]),
            TimingStats::from_durations(&[]),
            MemoryStats { max_rss_kb: None },
            MemoryStats { max_rss_kb: None },
        );
    }

    // Get first issue ID
    let issue_id = issues[0]["id"].as_str().unwrap_or("beads-1");

    let mut br_durations = Vec::with_capacity(config.timed_runs);
    let mut bd_durations = Vec::with_capacity(config.timed_runs);

    // Warmup
    for i in 0..config.warmup_runs {
        let title = format!("Updated warmup {i}");
        let _ = workspace.run_br(["update", issue_id, "--title", &title]);
        let _ = workspace.run_bd(["update", issue_id, "--title", &title]);
    }

    // Timed runs
    for i in 0..config.timed_runs {
        let title = format!("Benchmark update {i}");

        let br_start = Instant::now();
        let _ = workspace.run_br(["update", issue_id, "--title", &title]);
        br_durations.push(br_start.elapsed());

        let bd_start = Instant::now();
        let _ = workspace.run_bd(["update", issue_id, "--title", &title]);
        bd_durations.push(bd_start.elapsed());
    }

    let args = ["update", issue_id, "--title", "RSS test"];
    let br_rss = measure_rss("br", &workspace.br_root, &args);
    let bd_rss = measure_rss("bd", &workspace.bd_root, &args);

    WorkloadResult::new(
        "update",
        TimingStats::from_durations(&br_durations),
        TimingStats::from_durations(&bd_durations),
        br_rss,
        bd_rss,
    )
}

fn benchmark_close_reopen(
    workspace: &DatasetBenchmarkWorkspace,
    config: &BenchConfig,
) -> WorkloadResult {
    // Create issues specifically for close/reopen testing
    let mut br_durations = Vec::with_capacity(config.timed_runs);
    let mut bd_durations = Vec::with_capacity(config.timed_runs);

    // Warmup with create + close cycle
    for i in 0..config.warmup_runs {
        let title = format!("Close warmup {i}");
        let br_out = workspace.run_br(["create", &title, "--type", "task", "--json"]);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&br_out.stdout) {
            if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                let _ = workspace.run_br(["close", id]);
            }
        }

        let bd_out = workspace.run_bd(["create", &title, "--type", "task", "--json"]);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&bd_out.stdout) {
            if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                let _ = workspace.run_bd(["close", id]);
            }
        }
    }

    // Timed runs: create then close
    for i in 0..config.timed_runs {
        // BR: create then close
        let br_title = format!("BR close bench {i}");
        let br_create = workspace.run_br(["create", &br_title, "--type", "task", "--json"]);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&br_create.stdout) {
            if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                let start = Instant::now();
                let _ = workspace.run_br(["close", id]);
                br_durations.push(start.elapsed());
            }
        }

        // BD: create then close
        let bd_title = format!("BD close bench {i}");
        let bd_create = workspace.run_bd(["create", &bd_title, "--type", "task", "--json"]);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&bd_create.stdout) {
            if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                let start = Instant::now();
                let _ = workspace.run_bd(["close", id]);
                bd_durations.push(start.elapsed());
            }
        }
    }

    // RSS for close (create issue first)
    let br_create = workspace.run_br(["create", "RSS close test br", "--type", "task", "--json"]);
    let br_rss = if let Ok(v) = serde_json::from_str::<serde_json::Value>(&br_create.stdout) {
        if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
            measure_rss("br", &workspace.br_root, &["close", id])
        } else {
            MemoryStats { max_rss_kb: None }
        }
    } else {
        MemoryStats { max_rss_kb: None }
    };

    let bd_create = workspace.run_bd(["create", "RSS close test bd", "--type", "task", "--json"]);
    let bd_rss = if let Ok(v) = serde_json::from_str::<serde_json::Value>(&bd_create.stdout) {
        if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
            measure_rss("bd", &workspace.bd_root, &["close", id])
        } else {
            MemoryStats { max_rss_kb: None }
        }
    } else {
        MemoryStats { max_rss_kb: None }
    };

    WorkloadResult::new(
        "close",
        TimingStats::from_durations(&br_durations),
        TimingStats::from_durations(&bd_durations),
        br_rss,
        bd_rss,
    )
}

// ============================================================================
// DATASET BENCHMARK RUNNER
// ============================================================================

fn benchmark_dataset(
    dataset: KnownDataset,
    config: &BenchConfig,
) -> Option<DatasetBenchmarkResult> {
    info!("benchmark_dataset: starting for {}", dataset.name());

    // Create integrity guard to ensure we don't mutate source
    let mut guard = match DatasetIntegrityGuard::new(dataset) {
        Ok(g) => g,
        Err(e) => {
            eprintln!(
                "  [SKIP] {} - failed to create integrity guard: {}",
                dataset.name(),
                e
            );
            return None;
        }
    };

    let before = guard.verify_before();
    if !before.passed {
        eprintln!(
            "  [SKIP] {} - source integrity check failed: {}",
            dataset.name(),
            before.message
        );
        return None;
    }

    // Create isolated workspace
    let workspace = match DatasetBenchmarkWorkspace::from_dataset(dataset) {
        Ok(w) => w,
        Err(e) => {
            eprintln!(
                "  [SKIP] {} - failed to create workspace: {}",
                dataset.name(),
                e
            );
            return None;
        }
    };

    let metadata = workspace.metadata();
    println!(
        "\n  Benchmarking: {} ({} issues)",
        metadata.name, metadata.issue_count
    );

    // Check if br is compatible with this dataset (some datasets have old issue ID formats)
    let br_check = workspace.run_br(["list", "--json"]);
    if !br_check.success {
        eprintln!(
            "  [SKIP] {} - br not compatible with this dataset: {}",
            dataset.name(),
            br_check.stderr.lines().next().unwrap_or("unknown error")
        );
        return None;
    }

    // Read-heavy workloads
    let read_workloads = vec![
        benchmark_list(&workspace, config),
        benchmark_search(&workspace, config),
        benchmark_ready(&workspace, config),
        benchmark_stats(&workspace, config),
        benchmark_blocked(&workspace, config),
    ];

    // Write-heavy workloads
    let write_workloads = vec![
        benchmark_create(&workspace, config),
        benchmark_update(&workspace, config),
        benchmark_close_reopen(&workspace, config),
    ];

    // Verify source integrity after (isolated copy was used, source should be untouched)
    let after = guard.verify_after();
    if !after.passed {
        eprintln!(
            "  [ERROR] {} - source was mutated during benchmark!",
            dataset.name()
        );
        return None;
    }

    Some(DatasetBenchmarkResult {
        dataset_name: metadata.name.clone(),
        issue_count: metadata.issue_count,
        db_size_bytes: metadata.db_size_bytes,
        read_workloads,
        write_workloads,
    })
}

// ============================================================================
// SUMMARY REPORT
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetBenchmarkReport {
    pub timestamp: String,
    pub config: BenchConfig,
    pub results: Vec<DatasetBenchmarkResult>,
}

impl DatasetBenchmarkReport {
    pub fn new(config: &BenchConfig, results: Vec<DatasetBenchmarkResult>) -> Self {
        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            config: config.clone(),
            results,
        }
    }

    pub fn print_summary(&self) {
        println!("\n");
        println!("╔══════════════════════════════════════════════════════════════╗");
        println!("║         REAL DATASET BENCHMARK COMPARISON REPORT            ║");
        println!("║                    br (Rust) vs bd (Go)                      ║");
        println!("╚══════════════════════════════════════════════════════════════╝");
        println!("Timestamp: {}", self.timestamp);
        println!(
            "Config: {} warmup, {} timed runs",
            self.config.warmup_runs, self.config.timed_runs
        );

        for result in &self.results {
            result.print_table();
        }

        // Aggregate summary
        self.print_aggregate_summary();
    }

    fn print_aggregate_summary(&self) {
        println!("\n══════════════════════════════════════════════════════════════");
        println!("AGGREGATE SUMMARY ACROSS ALL DATASETS");
        println!("══════════════════════════════════════════════════════════════");

        let mut total_read = 0;
        let mut br_faster_read = 0;
        let mut total_write = 0;
        let mut br_faster_write = 0;

        for result in &self.results {
            for workload in &result.read_workloads {
                total_read += 1;
                if workload.speedup_percent > 0.0 {
                    br_faster_read += 1;
                }
            }
            for workload in &result.write_workloads {
                total_write += 1;
                if workload.speedup_percent > 0.0 {
                    br_faster_write += 1;
                }
            }
        }

        println!(
            "\nRead-heavy workloads:  br faster in {}/{} ({:.0}%)",
            br_faster_read,
            total_read,
            if total_read > 0 {
                br_faster_read as f64 / total_read as f64 * 100.0
            } else {
                0.0
            }
        );
        println!(
            "Write-heavy workloads: br faster in {}/{} ({:.0}%)",
            br_faster_write,
            total_write,
            if total_write > 0 {
                br_faster_write as f64 / total_write as f64 * 100.0
            } else {
                0.0
            }
        );

        let total = total_read + total_write;
        let br_faster = br_faster_read + br_faster_write;
        println!(
            "\nOverall: br faster in {}/{} workloads ({:.0}%)",
            br_faster,
            total,
            if total > 0 {
                br_faster as f64 / total as f64 * 100.0
            } else {
                0.0
            }
        );
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

// ============================================================================
// BASELINE + REGRESSION CHECKS
// ============================================================================

fn workload_duration_ms(stats: &TimingStats) -> u128 {
    if stats.median_ms.is_finite() && stats.median_ms > 0.0 {
        stats.median_ms.round() as u128
    } else {
        0
    }
}

fn workload_ratio(br_stats: &TimingStats, bd_stats: &TimingStats) -> f64 {
    if bd_stats.median_ms > 0.0 {
        br_stats.median_ms / bd_stats.median_ms
    } else {
        1.0
    }
}

fn workload_rss_ratio(br_rss_kb: Option<u64>, bd_rss_kb: Option<u64>) -> Option<f64> {
    match (br_rss_kb, bd_rss_kb) {
        (Some(br), Some(bd)) if bd > 0 => Some(br as f64 / bd as f64),
        _ => None,
    }
}

fn collect_comparisons(
    result: &DatasetBenchmarkResult,
) -> Vec<(String, f64, u128, u128, Option<f64>)> {
    let mut comparisons = Vec::new();

    for workload in result
        .read_workloads
        .iter()
        .chain(result.write_workloads.iter())
    {
        let ratio = workload_ratio(&workload.br_stats, &workload.bd_stats);
        let br_ms = workload_duration_ms(&workload.br_stats);
        let bd_ms = workload_duration_ms(&workload.bd_stats);
        let rss_ratio = workload_rss_ratio(workload.br_rss_kb, workload.bd_rss_kb);

        comparisons.push((workload.name.clone(), ratio, br_ms, bd_ms, rss_ratio));
    }

    comparisons
}

fn run_regression_checks(results: &[DatasetBenchmarkResult]) {
    if results.is_empty() {
        return;
    }

    let config = RegressionConfig::from_env();
    let mut baselines = BaselineStore::load_or_default(&config.baseline_file);
    let update_baseline = should_update_baseline();
    let mut regression_results: Vec<RegressionResult> = Vec::new();

    for result in results {
        let comparisons = collect_comparisons(result);

        if update_baseline {
            update_baselines_from_results(
                &mut baselines,
                &result.dataset_name,
                result.issue_count,
                &comparisons,
            );
        }

        for (label, ratio, _br_ms, _bd_ms, rss_ratio) in comparisons {
            if let Some(baseline) = baselines.get_baseline(&result.dataset_name, &label) {
                regression_results.push(RegressionResult::check(
                    &label,
                    &result.dataset_name,
                    ratio,
                    rss_ratio,
                    baseline,
                    &config,
                ));
            } else {
                regression_results.push(RegressionResult::no_baseline(
                    &label,
                    &result.dataset_name,
                    ratio,
                    rss_ratio,
                ));
            }
        }
    }

    if update_baseline {
        if let Err(e) = baselines.save(&config.baseline_file) {
            eprintln!(
                "Warning: Failed to save baseline file {}: {e}",
                config.baseline_file.display()
            );
        } else {
            println!(
                "Benchmark baselines updated: {}",
                config.baseline_file.display()
            );
        }
    }

    let summary = RegressionSummary::from_results(regression_results, &config);
    summary.print_table();

    if config.strict_mode && !summary.passed {
        panic!("Benchmark regressions detected (strict mode)");
    }
}

// ============================================================================
// TEST FUNCTIONS
// ============================================================================

/// Full benchmark across all available datasets
#[test]
#[ignore]
fn benchmark_dataset_full() {
    init_test_logging();

    println!("\n");
    println!("════════════════════════════════════════════════════════════════");
    println!("STARTING REAL DATASET BENCHMARK SUITE");
    println!("════════════════════════════════════════════════════════════════");

    let config = BenchConfig {
        warmup_runs: 2,
        timed_runs: 5,
    };

    let registry = DatasetRegistry::new();
    let mut results = Vec::new();

    for dataset in KnownDataset::all() {
        if registry.is_available(*dataset) {
            if let Some(result) = benchmark_dataset(*dataset, &config) {
                results.push(result);
            }
        } else {
            println!("  [SKIP] {} - not available", dataset.name());
        }
    }

    let report = DatasetBenchmarkReport::new(&config, results);
    report.print_summary();
    run_regression_checks(&report.results);

    // Save JSON report
    let json_report = report.to_json();
    let report_path = std::env::temp_dir().join("br_bd_dataset_benchmark_report.json");
    if let Err(e) = fs::write(&report_path, &json_report) {
        eprintln!(
            "Warning: Could not save report to {}: {}",
            report_path.display(),
            e
        );
    } else {
        println!("\nJSON report saved to: {}", report_path.display());
    }
}

/// Quick benchmark on beads_rust only for CI
#[test]
fn benchmark_dataset_quick() {
    skip_if_no_bd!();
    init_test_logging();

    info!("benchmark_dataset_quick: starting");

    let config = BenchConfig {
        warmup_runs: 1,
        timed_runs: 3,
    };

    // Just benchmark beads_rust as a quick sanity check
    let result = benchmark_dataset(KnownDataset::BeadsRust, &config);
    assert!(result.is_some(), "BeadsRust benchmark should succeed");

    let result = result.unwrap();
    assert!(
        !result.read_workloads.is_empty(),
        "Should have read workloads"
    );
    assert!(
        !result.write_workloads.is_empty(),
        "Should have write workloads"
    );

    // Print results
    result.print_table();
    run_regression_checks(std::slice::from_ref(&result));

    info!("benchmark_dataset_quick: completed successfully");
}

/// Test that dataset benchmark infrastructure works
#[test]
fn benchmark_dataset_infrastructure_works() {
    skip_if_no_bd!();
    init_test_logging();

    info!("benchmark_dataset_infrastructure_works: testing DatasetBenchmarkWorkspace");

    // Test workspace creation using empty workspaces + init
    let workspace = DatasetBenchmarkWorkspace::empty().expect("should create empty workspace");

    assert!(workspace.br_root.exists());
    assert!(workspace.bd_root.exists());

    // Initialize br workspace
    let br_init = workspace.run_br(["init"]);
    assert!(
        br_init.success,
        "br init should succeed: {}",
        br_init.stderr
    );

    // Initialize bd workspace
    let bd_init = workspace.run_bd(["init"]);
    assert!(
        bd_init.success,
        "bd init should succeed: {}",
        bd_init.stderr
    );

    // Create a test issue in each
    let br_create = workspace.run_br(["create", "Test issue br", "--type", "task"]);
    assert!(
        br_create.success,
        "br create should succeed: {}",
        br_create.stderr
    );

    let bd_create = workspace.run_bd(["create", "Test issue bd", "--type", "task"]);
    assert!(
        bd_create.success,
        "bd create should succeed: {}",
        bd_create.stderr
    );

    // Test list commands
    let br_out = workspace.run_br(["list", "--json"]);
    assert!(br_out.success, "br list should succeed: {}", br_out.stderr);

    let bd_out = workspace.run_bd(["list", "--json"]);
    assert!(bd_out.success, "bd list should succeed: {}", bd_out.stderr);

    info!("benchmark_dataset_infrastructure_works: completed successfully");
}
