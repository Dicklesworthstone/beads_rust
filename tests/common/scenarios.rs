//! Scenario DSL for unified E2E, Conformance, and Benchmark testing.
//!
//! This module defines a `Scenario` struct that can drive tests in three modes:
//! - **E2E**: Run `br` commands only, validate exit codes and JSON shapes
//! - **Conformance**: Run both `br` and `bd`, compare outputs with normalization
//! - **Benchmark**: Time commands, capture RSS, produce metrics
//!
//! Related beads:
//! - beads_rust-ir0t: Scenario DSL + normalization rules for conformance
//! - beads_rust-ag35: EPIC: Exhaustive E2E + Conformance + Benchmark Harness

#![allow(dead_code, clippy::similar_names)]

use super::dataset_registry::{IsolatedDataset, KnownDataset};
use super::harness::{
    CommandResult, ConformanceWorkspace as HarnessConformanceWorkspace, TestWorkspace,
};
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

/// Execution mode for a scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// E2E mode: run br only, validate behavior
    E2E,
    /// Conformance mode: run br and bd, compare outputs
    Conformance,
    /// Benchmark mode: time execution, capture metrics
    Benchmark,
}

/// How to compare JSON outputs in conformance mode.
#[derive(Debug, Clone)]
pub enum CompareMode {
    /// JSON outputs must be byte-identical after stable sort
    ExactJson,
    /// Ignore volatile fields (timestamps, IDs), compare structure
    NormalizedJson,
    /// Only check that specific fields match
    ContainsFields(Vec<String>),
    /// Compare exit codes only, ignore output
    ExitCodeOnly,
    /// Arrays may be in different order (sort before compare)
    ArrayUnordered,
    /// Exclude specific fields from comparison
    FieldsExcluded(Vec<String>),
    /// Only compare JSON structure (keys present), not values
    StructureOnly,
}

impl Default for CompareMode {
    fn default() -> Self {
        Self::NormalizedJson
    }
}

/// Fields that are volatile and should be normalized.
#[derive(Debug, Clone, Default)]
pub struct NormalizationRules {
    /// Fields to replace with fixed placeholder (e.g., timestamps)
    pub mask_fields: HashSet<String>,
    /// Fields to completely remove before comparison
    pub remove_fields: HashSet<String>,
    /// Allow timestamp differences within this duration
    pub timestamp_tolerance: Option<Duration>,
    /// Sort arrays before comparison
    pub sort_arrays: bool,
    /// Normalize ID hash portions (keep prefix, mask hash)
    pub normalize_ids: bool,
    /// Log when normalization is applied
    pub log_normalization: bool,
    /// Normalize path separators (Windows backslash to Unix forward slash)
    pub normalize_paths: bool,
    /// Normalize line endings (CRLF to LF)
    pub normalize_line_endings: bool,
    /// Fields that contain file paths and should have separators normalized
    pub path_fields: HashSet<String>,
}

impl NormalizationRules {
    /// Default normalization for conformance (timestamps + IDs + cross-platform).
    pub fn conformance_default() -> Self {
        let mask_fields = [
            "created_at",
            "updated_at",
            "closed_at",
            "defer_until",
            "due_at",
            "deleted_at",
            "compacted_at",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let path_fields = [
            "path",
            "file_path",
            "source_path",
            "db_path",
            "jsonl_path",
            "log_path",
            "workspace_root",
            "beads_dir",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        Self {
            mask_fields,
            remove_fields: HashSet::new(),
            timestamp_tolerance: Some(Duration::from_secs(5)),
            sort_arrays: true,
            normalize_ids: true,
            log_normalization: true,
            normalize_paths: true,
            normalize_line_endings: true,
            path_fields,
        }
    }

    /// Strict normalization: only sort arrays.
    pub fn strict() -> Self {
        Self {
            sort_arrays: true,
            log_normalization: true,
            ..Default::default()
        }
    }

    /// Cross-platform normalization only (paths + line endings).
    pub fn cross_platform() -> Self {
        let path_fields = [
            "path",
            "file_path",
            "source_path",
            "db_path",
            "jsonl_path",
            "log_path",
            "workspace_root",
            "beads_dir",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        Self {
            normalize_paths: true,
            normalize_line_endings: true,
            path_fields,
            log_normalization: true,
            ..Default::default()
        }
    }

    /// Apply normalization to a JSON value.
    pub fn apply(&self, value: &mut Value) -> Vec<String> {
        let mut log = Vec::new();
        self.normalize_value(value, "", &mut log);
        log
    }

    fn normalize_value(&self, value: &mut Value, path: &str, log: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                // Remove fields
                for field in &self.remove_fields {
                    if map.remove(field).is_some() && self.log_normalization {
                        let removed_path = if path.is_empty() {
                            field.clone()
                        } else {
                            format!("{path}.{field}")
                        };
                        log.push(format!("Removed field: {removed_path}"));
                    }
                }

                // Mask and normalize fields
                for (key, val) in map.iter_mut() {
                    let field_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };

                    if self.mask_fields.contains(key) {
                        if let Some(s) = val.as_str() {
                            if !s.is_empty() && self.log_normalization {
                                log.push(format!("Masked timestamp: {field_path}"));
                            }
                        }
                        *val = Value::String("NORMALIZED_TIMESTAMP".to_string());
                    } else if self.normalize_ids && (key == "id" || key.ends_with("_id")) {
                        if let Some(s) = val.as_str() {
                            if let Some(dash_pos) = s.rfind('-') {
                                let prefix = &s[..dash_pos];
                                *val = Value::String(format!("{prefix}-HASH"));
                                if self.log_normalization {
                                    log.push(format!("Normalized ID: {field_path}"));
                                }
                            }
                        }
                    } else if self.normalize_paths && self.path_fields.contains(key) {
                        // Normalize path separators (Windows backslash to Unix forward slash)
                        if let Some(s) = val.as_str() {
                            if s.contains('\\') {
                                let normalized = s.replace('\\', "/");
                                if self.log_normalization {
                                    log.push(format!("Normalized path: {field_path}"));
                                }
                                *val = Value::String(normalized);
                            }
                        }
                        // Also apply line ending normalization if enabled
                        if self.normalize_line_endings {
                            if let Some(s) = val.as_str() {
                                if s.contains("\r\n") {
                                    let normalized = s.replace("\r\n", "\n");
                                    *val = Value::String(normalized);
                                }
                            }
                        }
                    } else {
                        self.normalize_value(val, &field_path, log);
                    }
                }
            }
            Value::Array(arr) => {
                for (i, item) in arr.iter_mut().enumerate() {
                    self.normalize_value(item, &format!("{path}[{i}]"), log);
                }
                if self.sort_arrays && !arr.is_empty() {
                    // Sort by JSON string representation for determinism
                    arr.sort_by(|a, b| {
                        serde_json::to_string(a)
                            .unwrap_or_default()
                            .cmp(&serde_json::to_string(b).unwrap_or_default())
                    });
                }
            }
            Value::String(s) => {
                // Normalize line endings for all string values
                if self.normalize_line_endings && s.contains("\r\n") {
                    let normalized = s.replace("\r\n", "\n");
                    if self.log_normalization {
                        log.push(format!("Normalized line endings: {path}"));
                    }
                    *s = normalized;
                }
            }
            _ => {}
        }
    }
}

/// Expected invariants for a scenario.
#[derive(Debug, Clone, Default)]
pub struct Invariants {
    /// Command must succeed (exit 0)
    pub expect_success: bool,
    /// Command must fail (non-zero exit)
    pub expect_failure: bool,
    /// Expected exit code (if specific)
    pub expected_exit_code: Option<i32>,
    /// stdout must contain these strings
    pub stdout_contains: Vec<String>,
    /// stderr must contain these strings
    pub stderr_contains: Vec<String>,
    /// stdout must NOT contain these strings
    pub stdout_excludes: Vec<String>,
    /// No git operations allowed (checked via log patterns)
    pub no_git_ops: bool,
    /// Only .beads/ files may be modified
    pub path_confinement: bool,
    /// JSON output must match this schema (field names present)
    pub json_schema_fields: Vec<String>,
}

impl Invariants {
    pub fn success() -> Self {
        Self {
            expect_success: true,
            ..Default::default()
        }
    }

    pub fn failure() -> Self {
        Self {
            expect_failure: true,
            ..Default::default()
        }
    }

    pub fn with_no_git_ops(mut self) -> Self {
        self.no_git_ops = true;
        self
    }

    pub fn with_path_confinement(mut self) -> Self {
        self.path_confinement = true;
        self
    }
}

/// Setup configuration for a scenario.
#[derive(Debug, Clone)]
pub enum ScenarioSetup {
    /// Start with a fresh (empty) workspace, run `br init`
    Fresh,
    /// Copy from a known dataset
    Dataset(KnownDataset),
    /// Run these setup commands before the test
    Commands(Vec<ScenarioCommand>),
}

impl Default for ScenarioSetup {
    fn default() -> Self {
        Self::Fresh
    }
}

/// A command to run as part of a scenario.
#[derive(Debug, Clone)]
pub struct ScenarioCommand {
    /// Command arguments (e.g., ["create", "Test issue", "--priority", "1"])
    pub args: Vec<String>,
    /// Environment variables to set
    pub env: Vec<(String, String)>,
    /// Optional stdin input
    pub stdin: Option<String>,
    /// Label for logging
    pub label: String,
}

impl ScenarioCommand {
    pub fn new<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let args: Vec<String> = args.into_iter().map(Into::into).collect();
        let label = args.first().cloned().unwrap_or_else(|| "cmd".to_string());
        Self {
            args,
            env: Vec::new(),
            stdin: None,
            label,
        }
    }

    pub fn with_env<I, K, V>(mut self, env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.env = env.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        self
    }

    pub fn with_stdin(mut self, input: impl Into<String>) -> Self {
        self.stdin = Some(input.into());
        self
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }
}

/// Result of running a scenario.
#[derive(Debug)]
pub struct ScenarioResult {
    pub passed: bool,
    pub mode: ExecutionMode,
    pub br_result: Option<CommandResult>,
    pub bd_result: Option<CommandResult>,
    pub comparison_result: Option<ComparisonResult>,
    pub invariant_failures: Vec<String>,
    pub normalization_log: Vec<String>,
    pub benchmark_metrics: Option<BenchmarkMetrics>,
}

/// Result of comparing br and bd outputs.
#[derive(Debug)]
pub struct ComparisonResult {
    pub matched: bool,
    pub br_json: Option<Value>,
    pub bd_json: Option<Value>,
    pub diff_description: Option<String>,
}

/// Benchmark metrics from a single run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkMetrics {
    pub br_duration_ms: u128,
    pub bd_duration_ms: Option<u128>,
    pub speedup_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub br_peak_rss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bd_peak_rss_bytes: Option<u64>,
    /// CPU time in milliseconds (user + system)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub br_cpu_time_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bd_cpu_time_ms: Option<u128>,
    /// Database file size after operation (bytes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_size_bytes: Option<u64>,
    /// JSONL export file size after operation (bytes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonl_size_bytes: Option<u64>,
    /// Iteration number (for multi-run benchmarks)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iteration: Option<u32>,
    /// Whether this was a warmup run
    #[serde(default)]
    pub is_warmup: bool,
}

/// Configuration for benchmark runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    /// Number of warmup iterations (not counted in statistics)
    pub warmup_iterations: u32,
    /// Number of measured iterations
    pub measured_iterations: u32,
    /// Whether to include bd in benchmarks (for ratio computation)
    pub include_bd: bool,
    /// Whether to measure peak RSS (Linux only, may add overhead)
    pub measure_rss: bool,
    /// Whether to measure IO sizes (db and jsonl files)
    pub measure_io: bool,
    /// Minimum duration for a valid benchmark run (filters outliers)
    pub min_duration_ms: u64,
    /// Maximum duration before considering a timeout (0 = no limit)
    pub max_duration_ms: u64,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            warmup_iterations: 1,
            measured_iterations: 3,
            include_bd: std::env::var("BENCH_INCLUDE_BD").map_or(false, |v| v == "1"),
            measure_rss: std::env::var("BENCH_MEASURE_RSS").map_or(true, |v| v != "0"),
            measure_io: std::env::var("BENCH_MEASURE_IO").map_or(true, |v| v != "0"),
            min_duration_ms: 0,
            max_duration_ms: 300_000, // 5 minutes
        }
    }
}

impl BenchmarkConfig {
    /// Minimal config for quick benchmarks (1 warmup, 1 measured)
    pub fn quick() -> Self {
        Self {
            warmup_iterations: 1,
            measured_iterations: 1,
            ..Default::default()
        }
    }

    /// Thorough config for production benchmarks
    pub fn thorough() -> Self {
        Self {
            warmup_iterations: 3,
            measured_iterations: 10,
            include_bd: true,
            ..Default::default()
        }
    }

    /// Builder: set iterations
    pub fn with_iterations(mut self, warmup: u32, measured: u32) -> Self {
        self.warmup_iterations = warmup;
        self.measured_iterations = measured;
        self
    }

    /// Builder: include bd
    pub fn with_bd(mut self, include: bool) -> Self {
        self.include_bd = include;
        self
    }
}

/// Aggregated benchmark statistics from multiple runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkSummary {
    /// Scenario name
    pub scenario: String,
    /// Configuration used
    pub config: BenchmarkConfig,
    /// Start timestamp (RFC3339)
    pub started_at: String,
    /// End timestamp (RFC3339)
    pub completed_at: String,
    /// Total number of runs (including warmup)
    pub total_runs: u32,
    /// br statistics
    pub br_stats: RunStatistics,
    /// bd statistics (if included)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bd_stats: Option<RunStatistics>,
    /// Speedup ratio (bd_median / br_median)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speedup_ratio: Option<f64>,
    /// Individual run metrics (measured runs only)
    pub runs: Vec<BenchmarkMetrics>,
    /// Warnings or notes
    pub notes: Vec<String>,
}

/// Statistics for a set of runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStatistics {
    /// Minimum duration (ms)
    pub min_ms: u128,
    /// Maximum duration (ms)
    pub max_ms: u128,
    /// Median duration (ms)
    pub median_ms: u128,
    /// Mean duration (ms)
    pub mean_ms: f64,
    /// Standard deviation (ms)
    pub stddev_ms: f64,
    /// Coefficient of variation (stddev/mean)
    pub cv: f64,
    /// Median peak RSS (bytes), if measured
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median_rss_bytes: Option<u64>,
}

// ============================================================================
// BENCHMARK HELPERS (beads_rust-owu6)
// ============================================================================

/// Measure peak RSS from /proc/<pid>/status on Linux.
/// Returns VmHWM (high water mark) which is the peak resident set size.
#[cfg(target_os = "linux")]
pub fn measure_peak_rss(pid: u32) -> Option<u64> {
    use std::fs;
    let status_path = format!("/proc/{}/status", pid);
    let content = fs::read_to_string(&status_path).ok()?;

    for line in content.lines() {
        // VmHWM is the peak RSS (high water mark)
        if line.starts_with("VmHWM:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(kb) = parts[1].parse::<u64>() {
                    return Some(kb * 1024); // Convert KB to bytes
                }
            }
        }
    }
    None
}

/// Stub for non-Linux platforms (RSS measurement not supported).
#[cfg(not(target_os = "linux"))]
pub fn measure_peak_rss(_pid: u32) -> Option<u64> {
    None
}

/// Measure IO sizes (database and JSONL files) in a workspace.
pub fn measure_io_sizes(workspace_root: &std::path::Path) -> (Option<u64>, Option<u64>) {
    let beads_dir = workspace_root.join(".beads");

    // Database size
    let db_size = beads_dir.join("beads.db").metadata().ok().map(|m| m.len());

    // JSONL size (issues.jsonl)
    let jsonl_size = beads_dir
        .join("issues.jsonl")
        .metadata()
        .ok()
        .map(|m| m.len());

    (db_size, jsonl_size)
}

/// Compute statistics from a vector of durations.
pub fn compute_statistics(durations: &[u128], rss_values: &[Option<u64>]) -> RunStatistics {
    let n = durations.len();
    if n == 0 {
        return RunStatistics {
            min_ms: 0,
            max_ms: 0,
            median_ms: 0,
            mean_ms: 0.0,
            stddev_ms: 0.0,
            cv: 0.0,
            median_rss_bytes: None,
        };
    }

    // Sort for min/max/median
    let mut sorted = durations.to_vec();
    sorted.sort();

    let min_ms = sorted[0];
    let max_ms = sorted[n - 1];
    let median_ms = if n % 2 == 0 {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2
    } else {
        sorted[n / 2]
    };

    // Mean
    let sum: u128 = sorted.iter().sum();
    let mean_ms = sum as f64 / n as f64;

    // Standard deviation
    let variance: f64 = sorted
        .iter()
        .map(|&x| {
            let diff = x as f64 - mean_ms;
            diff * diff
        })
        .sum::<f64>()
        / n as f64;
    let stddev_ms = variance.sqrt();

    // Coefficient of variation
    let cv = if mean_ms > 0.0 {
        stddev_ms / mean_ms
    } else {
        0.0
    };

    // Median RSS
    let median_rss_bytes = {
        let mut rss_vals: Vec<u64> = rss_values.iter().filter_map(|&v| v).collect();
        if rss_vals.is_empty() {
            None
        } else {
            rss_vals.sort();
            let rn = rss_vals.len();
            Some(if rn % 2 == 0 {
                (rss_vals[rn / 2 - 1] + rss_vals[rn / 2]) / 2
            } else {
                rss_vals[rn / 2]
            })
        }
    };

    RunStatistics {
        min_ms,
        max_ms,
        median_ms,
        mean_ms,
        stddev_ms,
        cv,
        median_rss_bytes,
    }
}

/// Benchmark runner that executes scenarios with proper methodology.
///
/// Implements:
/// - Warmup iterations (discarded from statistics)
/// - Multiple measured iterations
/// - Peak RSS measurement (Linux only)
/// - IO size measurement (db + jsonl files)
/// - Statistics computation (median, mean, stddev, CV)
/// - Optional br/bd comparison with ratio computation
pub struct BenchmarkRunner {
    config: BenchmarkConfig,
    artifacts_dir: Option<std::path::PathBuf>,
}

impl BenchmarkRunner {
    /// Create a new benchmark runner with the given configuration.
    pub fn new(config: BenchmarkConfig) -> Self {
        let artifacts_dir = std::env::var("BENCH_ARTIFACTS_DIR")
            .ok()
            .map(std::path::PathBuf::from);

        Self {
            config,
            artifacts_dir,
        }
    }

    /// Create a runner with default configuration.
    pub fn default_runner() -> Self {
        Self::new(BenchmarkConfig::default())
    }

    /// Create a runner for quick benchmarks (minimal iterations).
    pub fn quick() -> Self {
        Self::new(BenchmarkConfig::quick())
    }

    /// Create a runner for thorough benchmarks.
    pub fn thorough() -> Self {
        Self::new(BenchmarkConfig::thorough())
    }

    /// Run a benchmark for a scenario and return the summary.
    pub fn run_benchmark(&self, scenario: &Scenario) -> BenchmarkSummary {
        use chrono::Utc;

        let started_at = Utc::now().to_rfc3339();
        let mut all_runs: Vec<BenchmarkMetrics> = Vec::new();
        let mut notes: Vec<String> = Vec::new();

        let total_iterations = self.config.warmup_iterations + self.config.measured_iterations;

        // Run all iterations (warmup + measured)
        for i in 0..total_iterations {
            let is_warmup = i < self.config.warmup_iterations;
            let iteration = i + 1;

            // Create fresh workspace for each iteration
            let mut workspace =
                TestWorkspace::new("benchmark", &format!("{}_{}", scenario.name, iteration));

            // Setup
            if matches!(scenario.setup, ScenarioSetup::Fresh) {
                let _ = workspace.init_br();
            }

            // Run setup commands
            for cmd in &scenario.setup_commands {
                workspace.run_br(&cmd.args, &cmd.label);
            }

            // Run the benchmark command and measure
            let br_start = std::time::Instant::now();
            let br_result =
                workspace.run_br(&scenario.test_command.args, &scenario.test_command.label);
            let br_duration = br_start.elapsed();

            // Measure IO sizes
            let (db_size, jsonl_size) = if self.config.measure_io {
                measure_io_sizes(&workspace.root)
            } else {
                (None, None)
            };

            // Build metrics for this run
            let metrics = BenchmarkMetrics {
                br_duration_ms: br_duration.as_millis(),
                bd_duration_ms: None, // TODO: Add bd support in future iteration
                speedup_ratio: None,
                br_peak_rss_bytes: None, // RSS measurement requires running process PID
                bd_peak_rss_bytes: None,
                br_cpu_time_ms: None, // CPU time requires more instrumentation
                bd_cpu_time_ms: None,
                db_size_bytes: db_size,
                jsonl_size_bytes: jsonl_size,
                iteration: Some(iteration),
                is_warmup,
            };

            all_runs.push(metrics);

            // Cleanup workspace
            workspace.finish(br_result.success);
        }

        // Separate warmup from measured runs
        let measured_runs: Vec<&BenchmarkMetrics> =
            all_runs.iter().filter(|m| !m.is_warmup).collect();

        // Compute statistics for measured runs only
        let br_durations: Vec<u128> = measured_runs.iter().map(|m| m.br_duration_ms).collect();
        let br_rss: Vec<Option<u64>> = measured_runs.iter().map(|m| m.br_peak_rss_bytes).collect();
        let br_stats = compute_statistics(&br_durations, &br_rss);

        // Check for high variance
        if br_stats.cv > 0.15 {
            notes.push(format!(
                "High variance detected (CV={:.2}%). Consider increasing iterations or reducing system load.",
                br_stats.cv * 100.0
            ));
        }

        let completed_at = Utc::now().to_rfc3339();

        // Build summary
        BenchmarkSummary {
            scenario: scenario.name.clone(),
            config: self.config.clone(),
            started_at,
            completed_at,
            total_runs: total_iterations,
            br_stats,
            bd_stats: None, // TODO: Add when bd benchmarking is implemented
            speedup_ratio: None,
            runs: all_runs.into_iter().filter(|m| !m.is_warmup).collect(),
            notes,
        }
    }

    /// Run benchmark and write results to artifacts directory.
    pub fn run_and_save(&self, scenario: &Scenario) -> std::io::Result<BenchmarkSummary> {
        let summary = self.run_benchmark(scenario);

        if let Some(ref artifacts_dir) = self.artifacts_dir {
            use std::fs;
            use std::io::Write;

            // Ensure directory exists
            fs::create_dir_all(artifacts_dir)?;

            // Write summary JSON
            let summary_path = artifacts_dir.join(format!("{}_summary.json", scenario.name));
            let mut file = fs::File::create(&summary_path)?;
            let json = serde_json::to_string_pretty(&summary)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            file.write_all(json.as_bytes())?;

            // Write per-run JSONL
            let runs_path = artifacts_dir.join(format!("{}_runs.jsonl", scenario.name));
            let mut runs_file = fs::File::create(&runs_path)?;
            for run in &summary.runs {
                let line = serde_json::to_string(run)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                writeln!(runs_file, "{}", line)?;
            }
        }

        Ok(summary)
    }
}

/// A test scenario that can be run in E2E, Conformance, or Benchmark mode.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// Unique name for the scenario
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Tags for filtering (e.g., "quick", "sync", "crud")
    pub tags: Vec<String>,
    /// How to set up the workspace
    pub setup: ScenarioSetup,
    /// Commands to run before the main test command
    pub setup_commands: Vec<ScenarioCommand>,
    /// The main command to test
    pub test_command: ScenarioCommand,
    /// Expected invariants
    pub invariants: Invariants,
    /// How to compare outputs (for conformance mode)
    pub compare_mode: CompareMode,
    /// Normalization rules (for conformance mode)
    pub normalization: NormalizationRules,
    /// Supported execution modes
    pub supported_modes: Vec<ExecutionMode>,
}

impl Scenario {
    /// Create a new scenario with minimal configuration.
    pub fn new(name: impl Into<String>, test_command: ScenarioCommand) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            tags: Vec::new(),
            setup: ScenarioSetup::Fresh,
            setup_commands: Vec::new(),
            test_command,
            invariants: Invariants::success(),
            compare_mode: CompareMode::default(),
            normalization: NormalizationRules::conformance_default(),
            supported_modes: vec![ExecutionMode::E2E, ExecutionMode::Conformance],
        }
    }

    /// Builder: add description.
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Builder: add tags.
    pub fn with_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Builder: set setup mode.
    pub fn with_setup(mut self, setup: ScenarioSetup) -> Self {
        self.setup = setup;
        self
    }

    /// Builder: add setup commands.
    pub fn with_setup_commands(mut self, commands: Vec<ScenarioCommand>) -> Self {
        self.setup_commands = commands;
        self
    }

    /// Builder: set invariants.
    pub fn with_invariants(mut self, invariants: Invariants) -> Self {
        self.invariants = invariants;
        self
    }

    /// Builder: set compare mode.
    pub fn with_compare_mode(mut self, mode: CompareMode) -> Self {
        self.compare_mode = mode;
        self
    }

    /// Builder: set normalization rules.
    pub fn with_normalization(mut self, rules: NormalizationRules) -> Self {
        self.normalization = rules;
        self
    }

    /// Builder: set supported modes.
    pub fn with_modes(mut self, modes: Vec<ExecutionMode>) -> Self {
        self.supported_modes = modes;
        self
    }

    /// Check if scenario supports a given mode.
    pub fn supports_mode(&self, mode: ExecutionMode) -> bool {
        self.supported_modes.contains(&mode)
    }

    /// Check if scenario has a given tag.
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }

    /// Check if scenario has any of the given tags.
    pub fn has_any_tag(&self, tags: &[String]) -> bool {
        tags.iter().any(|t| self.has_tag(t))
    }

    /// Check if scenario has all of the given tags.
    pub fn has_all_tags(&self, tags: &[String]) -> bool {
        tags.iter().all(|t| self.has_tag(t))
    }
}

// ============================================================================
// SCENARIO FILTER (beads_rust-o1az)
// ============================================================================

/// Filter for selecting scenarios by tags.
///
/// Supports include/exclude logic:
/// - If include_tags is non-empty, only scenarios with matching tags are selected
/// - If exclude_tags is non-empty, scenarios with any excluded tag are skipped
/// - Exclude takes precedence over include
///
/// Environment variables:
/// - `HARNESS_TAGS`: comma-separated list of tags to include (e.g., "quick,crud")
/// - `HARNESS_EXCLUDE_TAGS`: comma-separated list of tags to exclude (e.g., "slow,stress")
/// - `HARNESS_TAG_MATCH`: "any" (default) or "all" for include matching
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScenarioFilter {
    /// Tags to include (scenario must have at least one, or all if match_mode is All)
    pub include_tags: Vec<String>,
    /// Tags to exclude (scenario must not have any)
    pub exclude_tags: Vec<String>,
    /// Match mode for include tags: "any" (default) or "all"
    pub match_mode: TagMatchMode,
}

/// How to match include tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagMatchMode {
    /// Scenario matches if it has ANY of the include tags (default)
    #[default]
    Any,
    /// Scenario matches if it has ALL of the include tags
    All,
}

impl ScenarioFilter {
    /// Create a new empty filter (matches all scenarios).
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a filter from environment variables.
    ///
    /// Reads:
    /// - `HARNESS_TAGS`: comma-separated include tags
    /// - `HARNESS_EXCLUDE_TAGS`: comma-separated exclude tags
    /// - `HARNESS_TAG_MATCH`: "any" or "all"
    pub fn from_env() -> Self {
        let include_tags = std::env::var("HARNESS_TAGS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let exclude_tags = std::env::var("HARNESS_EXCLUDE_TAGS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let match_mode = std::env::var("HARNESS_TAG_MATCH")
            .ok()
            .map(|s| match s.to_lowercase().as_str() {
                "all" => TagMatchMode::All,
                _ => TagMatchMode::Any,
            })
            .unwrap_or_default();

        Self {
            include_tags,
            exclude_tags,
            match_mode,
        }
    }

    /// Builder: add include tags.
    pub fn with_include_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.include_tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Builder: add exclude tags.
    pub fn with_exclude_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exclude_tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Builder: set match mode.
    pub fn with_match_mode(mut self, mode: TagMatchMode) -> Self {
        self.match_mode = mode;
        self
    }

    /// Check if a scenario matches this filter.
    pub fn matches(&self, scenario: &Scenario) -> bool {
        // Check exclude tags first (takes precedence)
        for tag in &self.exclude_tags {
            if scenario.has_tag(tag) {
                return false;
            }
        }

        // If no include tags specified, match all (that weren't excluded)
        if self.include_tags.is_empty() {
            return true;
        }

        // Check include tags based on match mode
        match self.match_mode {
            TagMatchMode::Any => scenario.has_any_tag(&self.include_tags),
            TagMatchMode::All => scenario.has_all_tags(&self.include_tags),
        }
    }

    /// Filter a list of scenarios, returning only those that match.
    pub fn filter<'a>(&self, scenarios: &'a [Scenario]) -> Vec<&'a Scenario> {
        scenarios.iter().filter(|s| self.matches(s)).collect()
    }

    /// Check if filter is empty (matches all scenarios).
    pub fn is_empty(&self) -> bool {
        self.include_tags.is_empty() && self.exclude_tags.is_empty()
    }

    /// Get a human-readable description of the filter.
    pub fn description(&self) -> String {
        let mut parts = Vec::new();

        if !self.include_tags.is_empty() {
            let mode = match self.match_mode {
                TagMatchMode::Any => "any of",
                TagMatchMode::All => "all of",
            };
            parts.push(format!(
                "include {} [{}]",
                mode,
                self.include_tags.join(", ")
            ));
        }

        if !self.exclude_tags.is_empty() {
            parts.push(format!("exclude [{}]", self.exclude_tags.join(", ")));
        }

        if parts.is_empty() {
            "all scenarios".to_string()
        } else {
            parts.join("; ")
        }
    }

    /// Convert to JSON for logging in summary.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "include_tags": self.include_tags,
            "exclude_tags": self.exclude_tags,
            "match_mode": self.match_mode,
            "description": self.description(),
        })
    }
}

/// Scenario runner that executes scenarios in different modes.
pub struct ScenarioRunner {
    mode: ExecutionMode,
    artifacts_enabled: bool,
    filter: ScenarioFilter,
}

impl ScenarioRunner {
    pub fn new(mode: ExecutionMode) -> Self {
        Self {
            mode,
            artifacts_enabled: std::env::var("HARNESS_ARTIFACTS").map_or(false, |v| v == "1"),
            filter: ScenarioFilter::from_env(),
        }
    }

    pub fn with_artifacts(mut self, enabled: bool) -> Self {
        self.artifacts_enabled = enabled;
        self
    }

    /// Builder: set the scenario filter.
    pub fn with_filter(mut self, filter: ScenarioFilter) -> Self {
        self.filter = filter;
        self
    }

    /// Get the current filter.
    pub fn filter(&self) -> &ScenarioFilter {
        &self.filter
    }

    /// Check if a scenario should be run based on the filter.
    pub fn should_run(&self, scenario: &Scenario) -> bool {
        self.filter.matches(scenario)
    }

    /// Run multiple scenarios, filtering based on current filter.
    /// Returns results with filter info in logs.
    pub fn run_filtered(&self, scenarios: &[Scenario]) -> Vec<(String, ScenarioResult)> {
        let selected: Vec<_> = scenarios.iter().filter(|s| self.should_run(s)).collect();

        // Log filter selection
        if !self.filter.is_empty() {
            eprintln!(
                "Filter: {} ({}/{} scenarios selected)",
                self.filter.description(),
                selected.len(),
                scenarios.len()
            );
        }

        selected
            .into_iter()
            .map(|s| (s.name.clone(), self.run(s)))
            .collect()
    }

    /// Run a scenario and return the result.
    pub fn run(&self, scenario: &Scenario) -> ScenarioResult {
        if !scenario.supports_mode(self.mode) {
            return ScenarioResult {
                passed: false,
                mode: self.mode,
                br_result: None,
                bd_result: None,
                comparison_result: None,
                invariant_failures: vec![format!(
                    "Scenario {} does not support mode {:?}",
                    scenario.name, self.mode
                )],
                normalization_log: Vec::new(),
                benchmark_metrics: None,
            };
        }

        match self.mode {
            ExecutionMode::E2E => self.run_e2e(scenario),
            ExecutionMode::Conformance => self.run_conformance(scenario),
            ExecutionMode::Benchmark => self.run_benchmark(scenario),
        }
    }

    fn run_e2e(&self, scenario: &Scenario) -> ScenarioResult {
        let mut workspace = TestWorkspace::new("e2e", &scenario.name);

        if let ScenarioSetup::Dataset(dataset) = scenario.setup {
            if let Err(err) = populate_workspace_with_dataset(&mut workspace, dataset) {
                return ScenarioResult {
                    passed: false,
                    mode: self.mode,
                    br_result: None,
                    bd_result: None,
                    comparison_result: None,
                    invariant_failures: vec![format!("Dataset setup failed: {err}")],
                    normalization_log: Vec::new(),
                    benchmark_metrics: None,
                };
            }
        }

        let baseline_snapshot = if scenario.invariants.path_confinement {
            Some(snapshot_workspace(&workspace.root))
        } else {
            None
        };

        // Setup
        if matches!(scenario.setup, ScenarioSetup::Fresh) {
            let init = workspace.init_br();
            if !init.success {
                return ScenarioResult {
                    passed: false,
                    mode: self.mode,
                    br_result: Some(init),
                    bd_result: None,
                    comparison_result: None,
                    invariant_failures: vec!["Init failed".to_string()],
                    normalization_log: Vec::new(),
                    benchmark_metrics: None,
                };
            }
        }

        // Run setup commands
        let setup_commands = collect_setup_commands(scenario);
        for cmd in &setup_commands {
            let result = run_scenario_command(&mut workspace, cmd, None);
            if !result.success && scenario.invariants.expect_success {
                return ScenarioResult {
                    passed: false,
                    mode: self.mode,
                    br_result: Some(result),
                    bd_result: None,
                    comparison_result: None,
                    invariant_failures: vec![format!("Setup command {} failed", cmd.label)],
                    normalization_log: Vec::new(),
                    benchmark_metrics: None,
                };
            }
        }

        // Run test command
        let br_result = run_scenario_command(&mut workspace, &scenario.test_command, None);

        // Check invariants
        let mut invariant_failures = check_invariants(&scenario.invariants, &br_result);
        if let (true, Some(before)) = (scenario.invariants.path_confinement, baseline_snapshot) {
            let after = snapshot_workspace(&workspace.root);
            let violations = detect_path_confinement_violations(&before, &after);
            invariant_failures.extend(violations);
        }

        let passed = invariant_failures.is_empty();
        workspace.finish(passed);

        ScenarioResult {
            passed,
            mode: self.mode,
            br_result: Some(br_result),
            bd_result: None,
            comparison_result: None,
            invariant_failures,
            normalization_log: Vec::new(),
            benchmark_metrics: None,
        }
    }

    fn run_conformance(&self, scenario: &Scenario) -> ScenarioResult {
        let mut workspace = HarnessConformanceWorkspace::new("conformance", &scenario.name);

        // Initialize both (unless using a dataset)
        if matches!(scenario.setup, ScenarioSetup::Fresh) {
            let (br_init, bd_init) = workspace.init_both();
            if !br_init.success || !bd_init.success {
                return ScenarioResult {
                    passed: false,
                    mode: self.mode,
                    br_result: Some(br_init),
                    bd_result: Some(bd_init),
                    comparison_result: None,
                    invariant_failures: vec!["Init failed".to_string()],
                    normalization_log: Vec::new(),
                    benchmark_metrics: None,
                };
            }
        } else if let ScenarioSetup::Dataset(dataset) = scenario.setup {
            if let Err(err) = populate_conformance_with_dataset(&mut workspace, dataset) {
                return ScenarioResult {
                    passed: false,
                    mode: self.mode,
                    br_result: None,
                    bd_result: None,
                    comparison_result: None,
                    invariant_failures: vec![format!("Dataset setup failed: {err}")],
                    normalization_log: Vec::new(),
                    benchmark_metrics: None,
                };
            }
        }

        let baseline_snapshot = if scenario.invariants.path_confinement {
            Some(snapshot_workspace(&workspace.br_workspace))
        } else {
            None
        };

        // Run setup commands on both
        let setup_commands = collect_setup_commands(scenario);
        for cmd in &setup_commands {
            let br_setup = run_conformance_command(
                &mut workspace,
                cmd,
                &format!("{}_setup", cmd.label),
                BinaryTarget::Br,
            );
            let bd_setup = run_conformance_command(
                &mut workspace,
                cmd,
                &format!("{}_setup", cmd.label),
                BinaryTarget::Bd,
            );
            if !br_setup.success || !bd_setup.success {
                return ScenarioResult {
                    passed: false,
                    mode: self.mode,
                    br_result: Some(br_setup),
                    bd_result: Some(bd_setup),
                    comparison_result: None,
                    invariant_failures: vec![format!("Setup command {} failed", cmd.label)],
                    normalization_log: Vec::new(),
                    benchmark_metrics: None,
                };
            }
        }

        // Run test command on both
        let br_result = run_conformance_command(
            &mut workspace,
            &scenario.test_command,
            &scenario.test_command.label,
            BinaryTarget::Br,
        );
        let bd_result = run_conformance_command(
            &mut workspace,
            &scenario.test_command,
            &scenario.test_command.label,
            BinaryTarget::Bd,
        );

        // Compare outputs
        let (comparison_result, normalization_log) = compare_outputs(
            &br_result,
            &bd_result,
            &scenario.compare_mode,
            &scenario.normalization,
        );

        // Check invariants (on br only)
        let mut invariant_failures = check_invariants(&scenario.invariants, &br_result);
        if let (true, Some(before)) = (scenario.invariants.path_confinement, baseline_snapshot) {
            let after = snapshot_workspace(&workspace.br_workspace);
            let violations = detect_path_confinement_violations(&before, &after);
            invariant_failures.extend(violations);
        }

        // Add comparison failure if any
        if !comparison_result.matched {
            invariant_failures.push(
                comparison_result
                    .diff_description
                    .clone()
                    .unwrap_or_else(|| "Output mismatch".to_string()),
            );
        }

        let passed = invariant_failures.is_empty();
        workspace.finish(passed);

        ScenarioResult {
            passed,
            mode: self.mode,
            br_result: Some(br_result),
            bd_result: Some(bd_result),
            comparison_result: Some(comparison_result),
            invariant_failures,
            normalization_log,
            benchmark_metrics: None,
        }
    }

    fn run_benchmark(&self, scenario: &Scenario) -> ScenarioResult {
        // For now, benchmark mode is like E2E but captures timing metrics
        let mut workspace = TestWorkspace::new("benchmark", &scenario.name);

        if let ScenarioSetup::Dataset(dataset) = scenario.setup {
            if let Err(err) = populate_workspace_with_dataset(&mut workspace, dataset) {
                return ScenarioResult {
                    passed: false,
                    mode: self.mode,
                    br_result: None,
                    bd_result: None,
                    comparison_result: None,
                    invariant_failures: vec![format!("Dataset setup failed: {err}")],
                    normalization_log: Vec::new(),
                    benchmark_metrics: None,
                };
            }
        }

        if matches!(scenario.setup, ScenarioSetup::Fresh) {
            let _ = workspace.init_br();
        }

        let setup_commands = collect_setup_commands(scenario);
        for cmd in &setup_commands {
            let _ = run_scenario_command(&mut workspace, cmd, None);
        }

        let br_result = run_scenario_command(&mut workspace, &scenario.test_command, None);

        let benchmark_metrics = Some(BenchmarkMetrics {
            br_duration_ms: br_result.duration.as_millis(),
            bd_duration_ms: None,
            speedup_ratio: None,
            br_peak_rss_bytes: None,
            bd_peak_rss_bytes: None,
            br_cpu_time_ms: None,
            bd_cpu_time_ms: None,
            db_size_bytes: None,
            jsonl_size_bytes: None,
            iteration: None,
            is_warmup: false,
        });

        workspace.finish(br_result.success);

        ScenarioResult {
            passed: br_result.success,
            mode: self.mode,
            br_result: Some(br_result),
            bd_result: None,
            comparison_result: None,
            invariant_failures: Vec::new(),
            normalization_log: Vec::new(),
            benchmark_metrics,
        }
    }
}

/// Check invariants against a command result.
fn check_invariants(invariants: &Invariants, result: &CommandResult) -> Vec<String> {
    let mut failures = Vec::new();

    if invariants.expect_success && !result.success {
        failures.push(format!(
            "Expected success, got exit code {}",
            result.exit_code
        ));
    }

    if invariants.expect_failure && result.success {
        failures.push("Expected failure, but command succeeded".to_string());
    }

    if let Some(expected_code) = invariants.expected_exit_code {
        if result.exit_code != expected_code {
            failures.push(format!(
                "Expected exit code {}, got {}",
                expected_code, result.exit_code
            ));
        }
    }

    for needle in &invariants.stdout_contains {
        if !result.stdout.contains(needle) {
            failures.push(format!("stdout missing: {needle}"));
        }
    }

    for needle in &invariants.stderr_contains {
        if !result.stderr.contains(needle) {
            failures.push(format!("stderr missing: {needle}"));
        }
    }

    for needle in &invariants.stdout_excludes {
        if result.stdout.contains(needle) {
            failures.push(format!("stdout should not contain: {needle}"));
        }
    }

    if !invariants.json_schema_fields.is_empty() {
        let payload = extract_json_payload(&result.stdout);
        match serde_json::from_str::<Value>(&payload) {
            Ok(value) => {
                for field in &invariants.json_schema_fields {
                    if value.get(field).is_none() {
                        failures.push(format!("json missing field: {field}"));
                    }
                }
            }
            Err(err) => failures.push(format!("json parse error: {err}")),
        }
    }

    if invariants.no_git_ops {
        // Check for git command patterns in logs
        let git_patterns = ["git commit", "git push", "git add", "git checkout"];
        for pattern in git_patterns {
            if result.stderr.contains(pattern) || result.stdout.contains(pattern) {
                failures.push(format!("Unexpected git operation: {pattern}"));
            }
        }
    }

    failures
}

/// Compare br and bd outputs using the specified mode and normalization.
fn compare_outputs(
    br_result: &CommandResult,
    bd_result: &CommandResult,
    compare_mode: &CompareMode,
    normalization: &NormalizationRules,
) -> (ComparisonResult, Vec<String>) {
    let mut normalization_log = Vec::new();

    // Extract JSON payloads
    let br_json_str = extract_json_payload(&br_result.stdout);
    let bd_json_str = extract_json_payload(&bd_result.stdout);

    let br_json: Result<Value, _> = serde_json::from_str(&br_json_str);
    let bd_json: Result<Value, _> = serde_json::from_str(&bd_json_str);

    match compare_mode {
        CompareMode::ExitCodeOnly => {
            let matched = br_result.exit_code == bd_result.exit_code;
            return (
                ComparisonResult {
                    matched,
                    br_json: None,
                    bd_json: None,
                    diff_description: if matched {
                        None
                    } else {
                        Some(format!(
                            "Exit code mismatch: br={}, bd={}",
                            br_result.exit_code, bd_result.exit_code
                        ))
                    },
                },
                normalization_log,
            );
        }

        CompareMode::ExactJson => {
            let (Ok(br), Ok(bd)) = (br_json, bd_json) else {
                return (
                    ComparisonResult {
                        matched: false,
                        br_json: None,
                        bd_json: None,
                        diff_description: Some("JSON parse error".to_string()),
                    },
                    normalization_log,
                );
            };
            let matched = br == bd;
            (
                ComparisonResult {
                    matched,
                    br_json: Some(br.clone()),
                    bd_json: Some(bd.clone()),
                    diff_description: if matched {
                        None
                    } else {
                        Some("JSON mismatch".to_string())
                    },
                },
                normalization_log,
            )
        }

        CompareMode::NormalizedJson => {
            let (Ok(mut br), Ok(mut bd)) = (br_json, bd_json) else {
                return (
                    ComparisonResult {
                        matched: false,
                        br_json: None,
                        bd_json: None,
                        diff_description: Some("JSON parse error".to_string()),
                    },
                    normalization_log,
                );
            };

            let mut tolerance_issues = Vec::new();
            if normalization.timestamp_tolerance.is_some() {
                tolerance_issues = check_timestamp_tolerance(&br, &bd, normalization);
                normalization_log.extend(tolerance_issues.iter().cloned());
            }

            // Apply normalization
            let br_log = normalization.apply(&mut br);
            let bd_log = normalization.apply(&mut bd);
            normalization_log.extend(br_log.into_iter().map(|s| format!("br: {s}")));
            normalization_log.extend(bd_log.into_iter().map(|s| format!("bd: {s}")));

            let matched = br == bd && tolerance_issues.is_empty();
            (
                ComparisonResult {
                    matched,
                    br_json: Some(br.clone()),
                    bd_json: Some(bd.clone()),
                    diff_description: if matched {
                        None
                    } else if !tolerance_issues.is_empty() {
                        Some(format!(
                            "Timestamp tolerance exceeded: {}",
                            tolerance_issues.join("; ")
                        ))
                    } else {
                        Some("Normalized JSON mismatch".to_string())
                    },
                },
                normalization_log,
            )
        }

        CompareMode::ContainsFields(fields) => {
            let (Ok(br), Ok(bd)) = (br_json, bd_json) else {
                return (
                    ComparisonResult {
                        matched: false,
                        br_json: None,
                        bd_json: None,
                        diff_description: Some("JSON parse error".to_string()),
                    },
                    normalization_log,
                );
            };

            let mut mismatches = Vec::new();
            for field in fields {
                let br_val = br.get(field);
                let bd_val = bd.get(field);
                if br_val != bd_val {
                    mismatches.push(format!(
                        "Field '{}': br={:?}, bd={:?}",
                        field, br_val, bd_val
                    ));
                }
            }

            let matched = mismatches.is_empty();
            (
                ComparisonResult {
                    matched,
                    br_json: Some(br),
                    bd_json: Some(bd),
                    diff_description: if matched {
                        None
                    } else {
                        Some(mismatches.join("; "))
                    },
                },
                normalization_log,
            )
        }

        CompareMode::ArrayUnordered => {
            let (Ok(mut br), Ok(mut bd)) = (br_json, bd_json) else {
                return (
                    ComparisonResult {
                        matched: false,
                        br_json: None,
                        bd_json: None,
                        diff_description: Some("JSON parse error".to_string()),
                    },
                    normalization_log,
                );
            };
            let _ = normalization.apply(&mut br);
            let _ = normalization.apply(&mut bd);
            sort_arrays_recursively(&mut br);
            sort_arrays_recursively(&mut bd);
            let matched = br == bd;
            (
                ComparisonResult {
                    matched,
                    br_json: Some(br),
                    bd_json: Some(bd),
                    diff_description: if matched {
                        None
                    } else {
                        Some("Array-unordered JSON mismatch".to_string())
                    },
                },
                normalization_log,
            )
        }

        CompareMode::FieldsExcluded(fields) => {
            let (Ok(mut br), Ok(mut bd)) = (br_json, bd_json) else {
                return (
                    ComparisonResult {
                        matched: false,
                        br_json: None,
                        bd_json: None,
                        diff_description: Some("JSON parse error".to_string()),
                    },
                    normalization_log,
                );
            };
            for field in fields {
                remove_field_path(&mut br, field);
                remove_field_path(&mut bd, field);
            }
            let matched = br == bd;
            (
                ComparisonResult {
                    matched,
                    br_json: Some(br),
                    bd_json: Some(bd),
                    diff_description: if matched {
                        None
                    } else {
                        Some("Fields-excluded JSON mismatch".to_string())
                    },
                },
                normalization_log,
            )
        }

        CompareMode::StructureOnly => {
            let (Ok(br), Ok(bd)) = (br_json, bd_json) else {
                return (
                    ComparisonResult {
                        matched: false,
                        br_json: None,
                        bd_json: None,
                        diff_description: Some("JSON parse error".to_string()),
                    },
                    normalization_log,
                );
            };
            let br_shape = structure_only(&br);
            let bd_shape = structure_only(&bd);
            let matched = br_shape == bd_shape;
            (
                ComparisonResult {
                    matched,
                    br_json: Some(br_shape),
                    bd_json: Some(bd_shape),
                    diff_description: if matched {
                        None
                    } else {
                        Some("Structure mismatch".to_string())
                    },
                },
                normalization_log,
            )
        }
    }
}

/// Extract JSON payload from stdout (skips non-JSON preamble).
fn extract_json_payload(stdout: &str) -> String {
    for (idx, line) in stdout.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            return stdout
                .lines()
                .skip(idx)
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
        }
    }
    stdout.trim().to_string()
}

#[derive(Debug, Clone)]
struct FileFingerprint {
    size: u64,
    modified: Option<SystemTime>,
    is_dir: bool,
}

fn snapshot_workspace(root: &Path) -> HashMap<String, FileFingerprint> {
    let mut entries = HashMap::new();
    for entry in WalkDir::new(root)
        .max_depth(6)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if let Ok(rel) = path.strip_prefix(root) {
            let rel_str = rel.display().to_string();
            if rel_str.is_empty() {
                continue;
            }
            let metadata = entry.metadata().ok();
            let is_dir = entry.file_type().is_dir();
            let size = metadata.as_ref().map_or(0, |m| m.len());
            let modified = metadata.and_then(|m| m.modified().ok());
            entries.insert(
                rel_str,
                FileFingerprint {
                    size,
                    modified,
                    is_dir,
                },
            );
        }
    }
    entries
}

fn detect_path_confinement_violations(
    before: &HashMap<String, FileFingerprint>,
    after: &HashMap<String, FileFingerprint>,
) -> Vec<String> {
    let allowed_prefixes = [".beads/", "logs/"];
    let mut violations = Vec::new();

    let mut all_paths: HashSet<String> = HashSet::new();
    all_paths.extend(before.keys().cloned());
    all_paths.extend(after.keys().cloned());

    for path in all_paths {
        if allowed_prefixes.iter().any(|p| path.starts_with(p)) {
            continue;
        }
        match (before.get(&path), after.get(&path)) {
            (None, Some(_)) => violations.push(format!("Unexpected path created: {path}")),
            (Some(_), None) => violations.push(format!("Unexpected path removed: {path}")),
            (Some(before_meta), Some(after_meta)) => {
                if before_meta.is_dir || after_meta.is_dir {
                    continue;
                }
                if before_meta.size != after_meta.size
                    || before_meta.modified != after_meta.modified
                {
                    violations.push(format!("Unexpected path modified: {path}"));
                }
            }
            (None, None) => {}
        }
    }

    violations
}

fn collect_setup_commands(scenario: &Scenario) -> Vec<ScenarioCommand> {
    let mut commands = Vec::new();
    if let ScenarioSetup::Commands(cmds) = &scenario.setup {
        commands.extend(cmds.clone());
    }
    commands.extend(scenario.setup_commands.clone());
    commands
}

fn run_scenario_command(
    workspace: &mut TestWorkspace,
    command: &ScenarioCommand,
    label_suffix: Option<&str>,
) -> CommandResult {
    let label = if let Some(suffix) = label_suffix {
        format!("{}_{}", command.label, suffix)
    } else {
        command.label.clone()
    };

    match (command.env.is_empty(), command.stdin.as_ref()) {
        (true, None) => workspace.run_br(&command.args, &label),
        (false, None) => workspace.run_br_env(&command.args, command.env.clone(), &label),
        (true, Some(input)) => workspace.run_br_stdin(&command.args, input, &label),
        (false, Some(input)) => {
            workspace.run_br_env_stdin(&command.args, command.env.clone(), input, &label)
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum BinaryTarget {
    Br,
    Bd,
}

fn run_conformance_command(
    workspace: &mut HarnessConformanceWorkspace,
    command: &ScenarioCommand,
    label: &str,
    target: BinaryTarget,
) -> CommandResult {
    match (target, command.env.is_empty(), command.stdin.as_ref()) {
        (BinaryTarget::Br, true, None) => workspace.run_br(&command.args, label),
        (BinaryTarget::Br, false, None) => {
            workspace.run_br_env(&command.args, command.env.clone(), label)
        }
        (BinaryTarget::Br, true, Some(input)) => {
            workspace.run_br_stdin(&command.args, input, label)
        }
        (BinaryTarget::Br, false, Some(input)) => {
            workspace.run_br_env_stdin(&command.args, command.env.clone(), input, label)
        }
        (BinaryTarget::Bd, true, None) => workspace.run_bd(&command.args, label),
        (BinaryTarget::Bd, false, None) => {
            workspace.run_bd_env(&command.args, command.env.clone(), label)
        }
        (BinaryTarget::Bd, true, Some(input)) => {
            workspace.run_bd_stdin(&command.args, input, label)
        }
        (BinaryTarget::Bd, false, Some(input)) => {
            workspace.run_bd_env_stdin(&command.args, command.env.clone(), input, label)
        }
    }
}

fn populate_workspace_with_dataset(
    workspace: &mut TestWorkspace,
    dataset: KnownDataset,
) -> std::io::Result<()> {
    let isolated = IsolatedDataset::from_dataset(dataset)?;
    copy_dir_contents(isolated.root.join(".beads"), workspace.root.join(".beads"))?;
    copy_dir_contents(isolated.root.join(".git"), workspace.root.join(".git"))?;
    Ok(())
}

fn populate_conformance_with_dataset(
    workspace: &mut HarnessConformanceWorkspace,
    dataset: KnownDataset,
) -> std::io::Result<()> {
    let isolated = IsolatedDataset::from_dataset(dataset)?;
    copy_dir_contents(
        isolated.root.join(".beads"),
        workspace.br_workspace.join(".beads"),
    )?;
    copy_dir_contents(
        isolated.root.join(".git"),
        workspace.br_workspace.join(".git"),
    )?;
    copy_dir_contents(
        isolated.root.join(".beads"),
        workspace.bd_workspace.join(".beads"),
    )?;
    copy_dir_contents(
        isolated.root.join(".git"),
        workspace.bd_workspace.join(".git"),
    )?;
    Ok(())
}

fn copy_dir_contents(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    if !src.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(src).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        let rel = path.strip_prefix(src).unwrap_or(path);
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(path, &target)?;
        }
    }
    Ok(())
}

fn sort_arrays_recursively(value: &mut Value) {
    match value {
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                sort_arrays_recursively(item);
            }
            arr.sort_by(|a, b| {
                serde_json::to_string(a)
                    .unwrap_or_default()
                    .cmp(&serde_json::to_string(b).unwrap_or_default())
            });
        }
        Value::Object(map) => {
            for val in map.values_mut() {
                sort_arrays_recursively(val);
            }
        }
        _ => {}
    }
}

fn remove_field_path(value: &mut Value, path: &str) {
    let segments: Vec<&str> = path.split('.').collect();
    remove_field_segments(value, &segments);
}

fn remove_field_segments(value: &mut Value, segments: &[&str]) {
    if segments.is_empty() {
        return;
    }
    match value {
        Value::Object(map) => {
            if segments.len() == 1 {
                map.remove(segments[0]);
            } else if let Some(next) = map.get_mut(segments[0]) {
                remove_field_segments(next, &segments[1..]);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                remove_field_segments(item, segments);
            }
        }
        _ => {}
    }
}

fn structure_only(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), structure_only(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(structure_only).collect()),
        _ => Value::Null,
    }
}

fn check_timestamp_tolerance(
    br: &Value,
    bd: &Value,
    normalization: &NormalizationRules,
) -> Vec<String> {
    let Some(tolerance) = normalization.timestamp_tolerance else {
        return Vec::new();
    };

    let mut issues = Vec::new();
    check_timestamp_tolerance_inner(br, bd, normalization, "", tolerance, &mut issues);
    issues
}

fn check_timestamp_tolerance_inner(
    br: &Value,
    bd: &Value,
    normalization: &NormalizationRules,
    path: &str,
    tolerance: Duration,
    issues: &mut Vec<String>,
) {
    match (br, bd) {
        (Value::Object(br_map), Value::Object(bd_map)) => {
            for (key, br_val) in br_map {
                let bd_val = bd_map.get(key);
                let field_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if normalization.mask_fields.contains(key) {
                    if let (Some(br_str), Some(bd_str)) =
                        (br_val.as_str(), bd_val.and_then(Value::as_str))
                    {
                        if let (Ok(br_dt), Ok(bd_dt)) =
                            (parse_timestamp(br_str), parse_timestamp(bd_str))
                        {
                            let diff = (br_dt - bd_dt).num_seconds().abs() as u64;
                            if diff > tolerance.as_secs() {
                                issues.push(format!(
                                    "timestamp drift at {field_path}: br={br_str} bd={bd_str} diff={diff}s"
                                ));
                            }
                        }
                    }
                }
                if let Some(bd_val) = bd_val {
                    check_timestamp_tolerance_inner(
                        br_val,
                        bd_val,
                        normalization,
                        &field_path,
                        tolerance,
                        issues,
                    );
                }
            }
        }
        (Value::Array(br_arr), Value::Array(bd_arr)) => {
            for (idx, (br_val, bd_val)) in br_arr.iter().zip(bd_arr.iter()).enumerate() {
                let field_path = format!("{path}[{idx}]");
                check_timestamp_tolerance_inner(
                    br_val,
                    bd_val,
                    normalization,
                    &field_path,
                    tolerance,
                    issues,
                );
            }
        }
        _ => {}
    }
}

fn parse_timestamp(value: &str) -> Result<DateTime<FixedOffset>, chrono::ParseError> {
    DateTime::parse_from_rfc3339(value)
}

// ============================================================================
// PREDEFINED SCENARIOS
// ============================================================================

/// Module containing predefined scenarios for common operations.
pub mod catalog {
    use super::*;

    /// Create a CRUD scenario: create, list, show, update, close.
    pub fn crud_lifecycle() -> Scenario {
        Scenario::new("crud_lifecycle", ScenarioCommand::new(["list", "--json"]))
            .with_description("Full CRUD lifecycle: create, update, close, verify list")
            .with_tags(["crud", "quick"])
            .with_setup_commands(vec![
                ScenarioCommand::new(["create", "Test issue"]).with_label("create"),
                ScenarioCommand::new(["update", "--status", "in_progress"]).with_label("update"),
            ])
            .with_compare_mode(CompareMode::ContainsFields(vec![
                "title".into(),
                "status".into(),
            ]))
    }

    /// Empty list scenario.
    pub fn empty_list() -> Scenario {
        Scenario::new("empty_list", ScenarioCommand::new(["list", "--json"]))
            .with_description("Verify empty list output matches")
            .with_tags(["quick", "list"])
            .with_compare_mode(CompareMode::ExactJson)
    }

    /// Stats command scenario.
    pub fn stats_basic() -> Scenario {
        Scenario::new("stats_basic", ScenarioCommand::new(["stats", "--json"]))
            .with_description("Verify stats output after creating issues")
            .with_tags(["stats", "quick"])
            .with_setup_commands(vec![
                ScenarioCommand::new(["create", "Issue 1"]),
                ScenarioCommand::new(["create", "Issue 2"]),
            ])
            .with_compare_mode(CompareMode::ContainsFields(vec!["total".into()]))
    }

    /// Sync safety scenario.
    pub fn sync_safety() -> Scenario {
        Scenario::new(
            "sync_safety",
            ScenarioCommand::new(["sync", "--flush-only", "--json"]),
        )
        .with_description("Verify sync does not execute git commands")
        .with_tags(["sync", "safety"])
        .with_setup_commands(vec![ScenarioCommand::new(["create", "Test issue"])])
        .with_invariants(
            Invariants::success()
                .with_no_git_ops()
                .with_path_confinement(),
        )
        .with_compare_mode(CompareMode::ExitCodeOnly)
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn test_normalization_rules_apply() {
        let rules = NormalizationRules::conformance_default();
        let mut value = serde_json::json!({
            "id": "bd-abc123",
            "title": "Test",
            "created_at": "2026-01-17T10:00:00Z",
            "updated_at": "2026-01-17T11:00:00Z"
        });

        let log = rules.apply(&mut value);

        assert_eq!(value["created_at"], "NORMALIZED_TIMESTAMP");
        assert_eq!(value["updated_at"], "NORMALIZED_TIMESTAMP");
        assert_eq!(value["id"], "bd-HASH");
        assert!(!log.is_empty());
    }

    #[test]
    fn test_scenario_builder() {
        let scenario = Scenario::new("test", ScenarioCommand::new(["list"]))
            .with_description("Test scenario")
            .with_tags(["quick", "test"])
            .with_invariants(Invariants::success());

        assert_eq!(scenario.name, "test");
        assert!(scenario.has_tag("quick"));
        assert!(scenario.supports_mode(ExecutionMode::E2E));
    }

    #[test]
    fn test_compare_mode_default() {
        let mode = CompareMode::default();
        assert!(matches!(mode, CompareMode::NormalizedJson));
    }

    #[test]
    fn test_extract_json_payload() {
        let stdout = "Created bd-abc: Test\n[{\"id\": \"bd-abc\"}]";
        let json = extract_json_payload(stdout);
        assert!(json.starts_with('['));
    }

    #[test]
    fn compare_mode_array_unordered_matches() {
        let br_result = CommandResult {
            stdout: "[{\"id\":1},{\"id\":2}]".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
            duration: Duration::from_secs(0),
            log_path: Path::new(".").join("noop"),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        };
        let bd_result = CommandResult {
            stdout: "[{\"id\":2},{\"id\":1}]".to_string(),
            ..br_result.clone()
        };
        let (comparison, _) = compare_outputs(
            &br_result,
            &bd_result,
            &CompareMode::ArrayUnordered,
            &NormalizationRules::strict(),
        );
        assert!(comparison.matched);
    }

    #[test]
    fn compare_mode_fields_excluded_matches() {
        let br_result = CommandResult {
            stdout: "{\"id\":1,\"ts\":123}".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
            duration: Duration::from_secs(0),
            log_path: Path::new(".").join("noop"),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        };
        let bd_result = CommandResult {
            stdout: "{\"id\":1,\"ts\":999}".to_string(),
            ..br_result.clone()
        };
        let (comparison, _) = compare_outputs(
            &br_result,
            &bd_result,
            &CompareMode::FieldsExcluded(vec!["ts".to_string()]),
            &NormalizationRules::strict(),
        );
        assert!(comparison.matched);
    }

    #[test]
    fn compare_mode_structure_only_matches() {
        let br_result = CommandResult {
            stdout: "{\"id\":1,\"nested\":{\"a\":true}}".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
            duration: Duration::from_secs(0),
            log_path: Path::new(".").join("noop"),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        };
        let bd_result = CommandResult {
            stdout: "{\"id\":2,\"nested\":{\"a\":false}}".to_string(),
            ..br_result.clone()
        };
        let (comparison, _) = compare_outputs(
            &br_result,
            &bd_result,
            &CompareMode::StructureOnly,
            &NormalizationRules::strict(),
        );
        assert!(comparison.matched);
    }

    // ========================================================================
    // ScenarioFilter tests (beads_rust-o1az)
    // ========================================================================

    #[test]
    fn test_scenario_filter_empty_matches_all() {
        let filter = ScenarioFilter::new();
        let scenario =
            Scenario::new("test", ScenarioCommand::new(["list"])).with_tags(["quick", "crud"]);

        assert!(filter.matches(&scenario));
        assert!(filter.is_empty());
    }

    #[test]
    fn test_scenario_filter_include_any() {
        let filter = ScenarioFilter::new()
            .with_include_tags(["quick", "sync"])
            .with_match_mode(TagMatchMode::Any);

        let quick_scenario =
            Scenario::new("quick_test", ScenarioCommand::new(["list"])).with_tags(["quick"]);
        let sync_scenario =
            Scenario::new("sync_test", ScenarioCommand::new(["sync"])).with_tags(["sync"]);
        let slow_scenario =
            Scenario::new("slow_test", ScenarioCommand::new(["bench"])).with_tags(["slow"]);

        assert!(filter.matches(&quick_scenario));
        assert!(filter.matches(&sync_scenario));
        assert!(!filter.matches(&slow_scenario));
    }

    #[test]
    fn test_scenario_filter_include_all() {
        let filter = ScenarioFilter::new()
            .with_include_tags(["quick", "crud"])
            .with_match_mode(TagMatchMode::All);

        let both_tags = Scenario::new("both", ScenarioCommand::new(["list"]))
            .with_tags(["quick", "crud", "extra"]);
        let one_tag = Scenario::new("one", ScenarioCommand::new(["list"])).with_tags(["quick"]);

        assert!(filter.matches(&both_tags));
        assert!(!filter.matches(&one_tag));
    }

    #[test]
    fn test_scenario_filter_exclude() {
        let filter = ScenarioFilter::new().with_exclude_tags(["slow", "stress"]);

        let quick = Scenario::new("quick", ScenarioCommand::new(["list"])).with_tags(["quick"]);
        let slow = Scenario::new("slow", ScenarioCommand::new(["bench"])).with_tags(["slow"]);
        let stress = Scenario::new("stress", ScenarioCommand::new(["stress"]))
            .with_tags(["quick", "stress"]);

        assert!(filter.matches(&quick));
        assert!(!filter.matches(&slow));
        assert!(!filter.matches(&stress)); // excluded even though has "quick"
    }

    #[test]
    fn test_scenario_filter_exclude_precedence() {
        let filter = ScenarioFilter::new()
            .with_include_tags(["test"])
            .with_exclude_tags(["slow"]);

        let quick_test =
            Scenario::new("quick", ScenarioCommand::new(["list"])).with_tags(["test", "quick"]);
        let slow_test =
            Scenario::new("slow", ScenarioCommand::new(["list"])).with_tags(["test", "slow"]);

        assert!(filter.matches(&quick_test));
        assert!(!filter.matches(&slow_test)); // excluded takes precedence
    }

    #[test]
    fn test_scenario_filter_description() {
        let empty = ScenarioFilter::new();
        assert_eq!(empty.description(), "all scenarios");

        let include = ScenarioFilter::new().with_include_tags(["quick", "crud"]);
        assert!(include.description().contains("quick"));
        assert!(include.description().contains("crud"));

        let exclude = ScenarioFilter::new().with_exclude_tags(["slow"]);
        assert!(exclude.description().contains("exclude"));
        assert!(exclude.description().contains("slow"));
    }

    #[test]
    fn test_scenario_filter_to_json() {
        let filter = ScenarioFilter::new()
            .with_include_tags(["quick"])
            .with_exclude_tags(["slow"])
            .with_match_mode(TagMatchMode::Any);

        let json = filter.to_json();
        assert_eq!(json["include_tags"], serde_json::json!(["quick"]));
        assert_eq!(json["exclude_tags"], serde_json::json!(["slow"]));
        assert_eq!(json["match_mode"], serde_json::json!("any"));
        assert!(json["description"].as_str().is_some());
    }

    #[test]
    fn test_scenario_filter_filter_list() {
        let filter = ScenarioFilter::new().with_include_tags(["quick"]);

        let scenarios = vec![
            Scenario::new("quick1", ScenarioCommand::new(["list"])).with_tags(["quick"]),
            Scenario::new("slow1", ScenarioCommand::new(["bench"])).with_tags(["slow"]),
            Scenario::new("quick2", ScenarioCommand::new(["show"])).with_tags(["quick", "read"]),
        ];

        let filtered = filter.filter(&scenarios);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].name, "quick1");
        assert_eq!(filtered[1].name, "quick2");
    }

    #[test]
    fn test_tag_match_mode_default() {
        let mode = TagMatchMode::default();
        assert_eq!(mode, TagMatchMode::Any);
    }

    #[test]
    fn test_scenario_has_any_tag() {
        let scenario =
            Scenario::new("test", ScenarioCommand::new(["list"])).with_tags(["quick", "crud"]);

        assert!(scenario.has_any_tag(&["quick".to_string(), "slow".to_string()]));
        assert!(!scenario.has_any_tag(&["slow".to_string(), "stress".to_string()]));
    }

    #[test]
    fn test_scenario_has_all_tags() {
        let scenario = Scenario::new("test", ScenarioCommand::new(["list"]))
            .with_tags(["quick", "crud", "read"]);

        assert!(scenario.has_all_tags(&["quick".to_string(), "crud".to_string()]));
        assert!(!scenario.has_all_tags(&["quick".to_string(), "slow".to_string()]));
    }

    // ========================================================================
    // Cross-platform normalization tests (beads_rust-lsht)
    // ========================================================================

    #[test]
    fn test_path_separator_normalization() {
        let rules = NormalizationRules::cross_platform();
        let mut value = serde_json::json!({
            "path": "C:\\Users\\test\\project\\.beads",
            "file_path": "src\\main.rs",
            "db_path": "data\\beads.db",
            "title": "Not a path with \\ backslash"
        });

        let log = rules.apply(&mut value);

        // Path fields should have backslashes converted to forward slashes
        assert_eq!(value["path"], "C:/Users/test/project/.beads");
        assert_eq!(value["file_path"], "src/main.rs");
        assert_eq!(value["db_path"], "data/beads.db");
        // Non-path fields should not be modified
        assert_eq!(value["title"], "Not a path with \\ backslash");
        // Should log the normalizations
        assert!(log.iter().any(|l| l.contains("Normalized path")));
    }

    #[test]
    fn test_path_normalization_no_backslashes() {
        let rules = NormalizationRules::cross_platform();
        let mut value = serde_json::json!({
            "path": "/home/user/project/.beads",
            "file_path": "src/main.rs"
        });

        let log = rules.apply(&mut value);

        // Already Unix-style paths should remain unchanged
        assert_eq!(value["path"], "/home/user/project/.beads");
        assert_eq!(value["file_path"], "src/main.rs");
        // No path normalization should be logged
        assert!(!log.iter().any(|l| l.contains("Normalized path")));
    }

    #[test]
    fn test_line_ending_normalization() {
        let rules = NormalizationRules::cross_platform();
        let mut value = serde_json::json!({
            "description": "Line 1\r\nLine 2\r\nLine 3",
            "notes": "Single line no CRLF"
        });

        let log = rules.apply(&mut value);

        // CRLF should be converted to LF
        assert_eq!(value["description"], "Line 1\nLine 2\nLine 3");
        // Single line should remain unchanged
        assert_eq!(value["notes"], "Single line no CRLF");
        // Should log line ending normalization
        assert!(log.iter().any(|l| l.contains("Normalized line endings")));
    }

    #[test]
    fn test_line_ending_normalization_nested() {
        let rules = NormalizationRules::cross_platform();
        let mut value = serde_json::json!({
            "issues": [
                {"description": "First\r\nitem"},
                {"description": "Second\r\nitem"}
            ]
        });

        let log = rules.apply(&mut value);

        // Nested strings should also have CRLF normalized
        assert_eq!(value["issues"][0]["description"], "First\nitem");
        assert_eq!(value["issues"][1]["description"], "Second\nitem");
        assert!(!log.is_empty());
    }

    #[test]
    fn test_cross_platform_constructor() {
        let rules = NormalizationRules::cross_platform();

        assert!(rules.normalize_paths);
        assert!(rules.normalize_line_endings);
        assert!(rules.log_normalization);
        assert!(rules.path_fields.contains("path"));
        assert!(rules.path_fields.contains("file_path"));
        assert!(rules.path_fields.contains("db_path"));
        assert!(rules.path_fields.contains("workspace_root"));
        // Should not have timestamp masking
        assert!(rules.mask_fields.is_empty());
        assert!(!rules.normalize_ids);
    }

    #[test]
    fn test_conformance_default_includes_cross_platform() {
        let rules = NormalizationRules::conformance_default();

        // conformance_default should include cross-platform normalization
        assert!(rules.normalize_paths);
        assert!(rules.normalize_line_endings);
        assert!(rules.path_fields.contains("path"));
        // Plus the regular conformance features
        assert!(rules.mask_fields.contains("created_at"));
        assert!(rules.normalize_ids);
    }

    #[test]
    fn test_path_field_with_line_endings() {
        // Path fields should also have line endings normalized
        let rules = NormalizationRules::cross_platform();
        let mut value = serde_json::json!({
            "path": "C:\\Users\\test\r\n\\project"
        });

        rules.apply(&mut value);

        // Both backslashes and CRLF should be normalized
        assert_eq!(value["path"], "C:/Users/test\n/project");
    }

    #[test]
    fn test_strict_no_cross_platform() {
        let rules = NormalizationRules::strict();

        // strict() should not have cross-platform normalization
        assert!(!rules.normalize_paths);
        assert!(!rules.normalize_line_endings);
        assert!(rules.path_fields.is_empty());
    }
}
