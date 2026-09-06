//! Benchmark baseline and regression detection for real dataset benchmarks.
//!
//! This module provides:
//! - Baseline storage and loading (per operation/dataset expected metrics)
//! - Regression detection with configurable thresholds
//! - Environment variable configuration for CI vs local runs
//!
//! # Configuration
//!
//! Thresholds can be configured via environment variables:
//! - `BENCH_DURATION_THRESHOLD`: Max allowed duration increase (default: 1.20 = 20%)
//! - `BENCH_RSS_THRESHOLD`: Max allowed RSS increase (default: 1.30 = 30%)
//! - `BENCH_BASELINE_FILE`: Path to baseline JSON file (default: target/benchmark-results/baseline.json)
//! - `BENCH_STRICT_MODE`: If "1", any regression is a failure (default: "0" for warnings)
//!
//! # Usage
//!
//! ```ignore
//! let config = RegressionConfig::from_env();
//! let baselines = BaselineStore::load_or_default(&config.baseline_file);
//! let result = baselines.check_regression("list", "beads_rust", &comparison, &config);
//! println!("{}", result);
//! ```

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for regression detection, populated from environment variables.
#[derive(Debug, Clone)]
pub struct RegressionConfig {
    /// Max allowed ratio increase for duration (br/bd) before flagging as regression.
    /// Default: 1.20 (20% slower than baseline is a regression)
    pub duration_threshold: f64,

    /// Max allowed ratio increase for RSS before flagging as regression.
    /// Default: 1.30 (30% more memory than baseline is a regression)
    pub rss_threshold: f64,

    /// Path to baseline JSON file.
    pub baseline_file: PathBuf,

    /// If true, any regression causes test failure. Otherwise just warns.
    pub strict_mode: bool,
}

impl Default for RegressionConfig {
    fn default() -> Self {
        Self {
            duration_threshold: 1.20, // 20% regression allowed
            rss_threshold: 1.30,      // 30% memory regression allowed
            baseline_file: PathBuf::from("target/benchmark-results/baseline.json"),
            strict_mode: false,
        }
    }
}

impl RegressionConfig {
    /// Load configuration from environment variables.
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(val) = env::var("BENCH_DURATION_THRESHOLD")
            && let Ok(threshold) = val.parse::<f64>()
        {
            config.duration_threshold = threshold;
        }

        if let Ok(val) = env::var("BENCH_RSS_THRESHOLD")
            && let Ok(threshold) = val.parse::<f64>()
        {
            config.rss_threshold = threshold;
        }

        if let Ok(val) = env::var("BENCH_BASELINE_FILE") {
            config.baseline_file = PathBuf::from(val);
        }

        if let Ok(val) = env::var("BENCH_STRICT_MODE") {
            config.strict_mode = val == "1" || val.eq_ignore_ascii_case("true");
        }

        config
    }

    /// Create a config for CI (stricter thresholds).
    #[allow(dead_code)]
    pub fn ci() -> Self {
        Self {
            duration_threshold: 1.10, // 10% regression in CI
            rss_threshold: 1.20,      // 20% memory regression in CI
            strict_mode: true,
            ..Self::default()
        }
    }
}

// =============================================================================
// Baseline Storage
// =============================================================================

/// Expected baseline metrics for a single operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationBaseline {
    /// Expected br/bd duration ratio.
    pub duration_ratio: f64,

    /// Expected br/bd RSS ratio (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_ratio: Option<f64>,

    /// Absolute br duration in ms (for reference).
    pub br_duration_ms: u128,

    /// Absolute bd duration in ms (for reference).
    pub bd_duration_ms: u128,

    /// When this baseline was captured.
    pub captured_at: String,

    /// Optional notes about this baseline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Dataset-level baselines containing operation baselines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetBaseline {
    /// Dataset name.
    pub name: String,

    /// Issue count at baseline capture time.
    pub issue_count: usize,

    /// Operation baselines keyed by operation label.
    pub operations: HashMap<String, OperationBaseline>,
}

/// Store of all baselines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineStore {
    /// Version for forward compatibility.
    pub version: String,

    /// When this baseline store was last updated.
    pub updated_at: String,

    /// Dataset baselines keyed by dataset name.
    pub datasets: HashMap<String, DatasetBaseline>,
}

impl Default for BaselineStore {
    fn default() -> Self {
        Self {
            version: "1.0".to_string(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            datasets: HashMap::new(),
        }
    }
}

impl BaselineStore {
    /// Load an existing baseline, preserving missing/corrupt input as an error.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let content = fs::read_to_string(path)?;
        let store: Self = serde_json::from_str(&content)?;
        if store.version != "1.0" {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported baseline version: {}", store.version),
            ));
        }
        Ok(store)
    }

    /// Load baselines, retaining an empty, non-passing store on load failure.
    /// Call `load` when the caller needs the exact I/O or parsing error.
    pub fn load_or_default(path: &Path) -> Self {
        match Self::load(path) {
            Ok(store) => store,
            Err(error) => {
                eprintln!(
                    "Inconclusive: cannot load baseline {}: {error}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Save baselines to file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::create(path)?;
        let writer = BufWriter::new(file);
        serde_json::to_writer_pretty(writer, self)?;
        Ok(())
    }

    /// Get baseline for a specific operation in a dataset.
    pub fn get_baseline(&self, dataset: &str, operation: &str) -> Option<&OperationBaseline> {
        self.datasets
            .get(dataset)
            .and_then(|d| d.operations.get(operation))
    }

    /// Set baseline for an operation.
    pub fn set_baseline(
        &mut self,
        dataset: &str,
        issue_count: usize,
        operation: &str,
        baseline: OperationBaseline,
    ) {
        self.updated_at = chrono::Utc::now().to_rfc3339();

        let dataset_baseline =
            self.datasets
                .entry(dataset.to_string())
                .or_insert_with(|| DatasetBaseline {
                    name: dataset.to_string(),
                    issue_count,
                    operations: HashMap::new(),
                });

        dataset_baseline
            .operations
            .insert(operation.to_string(), baseline);
    }
}

// =============================================================================
// Regression Detection
// =============================================================================

/// Result of a regression check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionResult {
    /// Operation label.
    pub operation: String,

    /// Dataset name.
    pub dataset: String,

    /// Whether this is a regression.
    pub is_regression: bool,

    /// Regression status, including inconclusive evidence.
    pub status: RegressionStatus,

    /// Current duration ratio.
    pub current_ratio: f64,

    /// Baseline duration ratio (if available).
    pub baseline_ratio: Option<f64>,

    /// Percentage change from baseline.
    pub change_pct: Option<f64>,

    /// Current RSS ratio (br/bd), if available.
    pub current_rss_ratio: Option<f64>,

    /// Baseline RSS ratio (if available).
    pub baseline_rss_ratio: Option<f64>,

    /// Percentage change in RSS from baseline.
    pub rss_change_pct: Option<f64>,

    /// Human-readable reason for the status.
    pub reason: String,
}

/// Regression status levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegressionStatus {
    Ok,
    Warning,
    Regression,
    Inconclusive,
}

fn comparable_ratios(
    current: f64,
    current_rss: Option<f64>,
    baseline: &OperationBaseline,
    config: &RegressionConfig,
) -> bool {
    positive_finite(current)
        && positive_finite(baseline.duration_ratio)
        && positive_finite(current / baseline.duration_ratio)
        && ((current / baseline.duration_ratio - 1.0) * 100.0).is_finite()
        && config.duration_threshold.is_finite()
        && config.duration_threshold >= 1.0
        && config.rss_threshold.is_finite()
        && config.rss_threshold >= 1.0
        && match (current_rss, baseline.rss_ratio) {
            (None, None) => true,
            (Some(current), Some(reference)) => {
                positive_finite(current)
                    && positive_finite(reference)
                    && positive_finite(current / reference)
                    && ((current / reference - 1.0) * 100.0).is_finite()
            }
            _ => false,
        }
}

impl std::fmt::Display for RegressionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => write!(f, "ok"),
            Self::Warning => write!(f, "warning"),
            Self::Regression => write!(f, "REGRESSION"),
            Self::Inconclusive => write!(f, "INCONCLUSIVE"),
        }
    }
}

impl RegressionResult {
    /// Create result for when no baseline exists.
    pub fn no_baseline(
        operation: &str,
        dataset: &str,
        current_ratio: f64,
        current_rss_ratio: Option<f64>,
    ) -> Self {
        Self {
            operation: operation.to_string(),
            dataset: dataset.to_string(),
            is_regression: false,
            status: RegressionStatus::Inconclusive,
            current_ratio,
            baseline_ratio: None,
            change_pct: None,
            current_rss_ratio,
            baseline_rss_ratio: None,
            rss_change_pct: None,
            reason: "No baseline established yet".to_string(),
        }
    }

    /// Check if current metrics exceed thresholds compared to baseline.
    pub fn check(
        operation: &str,
        dataset: &str,
        current_ratio: f64,
        current_rss_ratio: Option<f64>,
        baseline: &OperationBaseline,
        config: &RegressionConfig,
    ) -> Self {
        let baseline_ratio = baseline.duration_ratio;
        if !comparable_ratios(current_ratio, current_rss_ratio, baseline, config) {
            let mut result =
                Self::no_baseline(operation, dataset, current_ratio, current_rss_ratio);
            result.baseline_ratio = Some(baseline_ratio);
            result.baseline_rss_ratio = baseline.rss_ratio;
            result.reason =
                "Invalid or unmatched metrics/thresholds; comparison is inconclusive".to_string();
            return result;
        }
        let ratio_change = current_ratio / baseline_ratio;
        let change_pct = (ratio_change - 1.0) * 100.0;

        let (duration_status, duration_reason) = if ratio_change <= 1.0 {
            // Improvement or same
            let improvement = (1.0 - ratio_change) * 100.0;
            (
                RegressionStatus::Ok,
                format!("{improvement:.1}% faster than baseline"),
            )
        } else if ratio_change <= config.duration_threshold {
            // Within threshold
            (
                RegressionStatus::Ok,
                format!(
                    "{change_pct:.1}% slower (within {:.0}% threshold)",
                    (config.duration_threshold - 1.0) * 100.0
                ),
            )
        } else {
            // Regression
            (
                RegressionStatus::Regression,
                format!(
                    "{change_pct:.1}% slower (exceeds {:.0}% threshold)",
                    (config.duration_threshold - 1.0) * 100.0
                ),
            )
        };

        let mut rss_regression = false;
        let mut rss_change_pct = None;
        let mut rss_reason = None::<String>;
        let baseline_rss_ratio = baseline.rss_ratio;

        if let (Some(current_rss), Some(baseline_rss)) = (current_rss_ratio, baseline_rss_ratio) {
            let rss_ratio_change = current_rss / baseline_rss;
            let rss_change = (rss_ratio_change - 1.0) * 100.0;
            rss_change_pct = Some(rss_change);

            if rss_ratio_change <= 1.0 {
                let improvement = (1.0 - rss_ratio_change) * 100.0;
                rss_reason = Some(format!("{improvement:.1}% lower RSS than baseline"));
            } else if rss_ratio_change <= config.rss_threshold {
                rss_reason = Some(format!(
                    "{rss_change:.1}% higher RSS (within {:.0}% threshold)",
                    (config.rss_threshold - 1.0) * 100.0
                ));
            } else {
                rss_regression = true;
                rss_reason = Some(format!(
                    "{rss_change:.1}% higher RSS (exceeds {:.0}% threshold)",
                    (config.rss_threshold - 1.0) * 100.0
                ));
            }
        }

        let status = if duration_status == RegressionStatus::Regression || rss_regression {
            RegressionStatus::Regression
        } else {
            RegressionStatus::Ok
        };

        let reason = if let Some(rss_reason) = rss_reason {
            format!("{duration_reason}; RSS: {rss_reason}")
        } else {
            duration_reason
        };

        Self {
            operation: operation.to_string(),
            dataset: dataset.to_string(),
            is_regression: status == RegressionStatus::Regression,
            status,
            current_ratio,
            baseline_ratio: Some(baseline_ratio),
            change_pct: Some(change_pct),
            current_rss_ratio,
            baseline_rss_ratio,
            rss_change_pct,
            reason,
        }
    }
}

// =============================================================================
// Regression Summary
// =============================================================================

/// Summary of regression checks for a benchmark run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionSummary {
    /// Total operations checked.
    pub total_operations: usize,

    /// Operations with regressions.
    pub regression_count: usize,

    /// Operations with warnings.
    pub warning_count: usize,

    /// Operations that passed.
    pub ok_count: usize,

    /// Operations without baselines.
    pub no_baseline_count: usize,

    /// Operations with unusable or missing comparison evidence.
    pub inconclusive_count: usize,

    /// Individual results.
    pub results: Vec<RegressionResult>,

    /// Whether every operation has usable evidence and the configured check passed.
    pub passed: bool,

    /// Config used for this check.
    pub config_summary: RegressionConfigSummary,
}

/// Serializable summary of regression config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionConfigSummary {
    pub duration_threshold: f64,
    pub rss_threshold: f64,
    pub strict_mode: bool,
}

impl From<&RegressionConfig> for RegressionConfigSummary {
    fn from(config: &RegressionConfig) -> Self {
        Self {
            duration_threshold: config.duration_threshold,
            rss_threshold: config.rss_threshold,
            strict_mode: config.strict_mode,
        }
    }
}

impl RegressionSummary {
    /// Create summary from individual results.
    pub fn from_results(results: Vec<RegressionResult>, config: &RegressionConfig) -> Self {
        let total_operations = results.len();
        let regression_count = results
            .iter()
            .filter(|r| r.status == RegressionStatus::Regression)
            .count();
        let warning_count = results
            .iter()
            .filter(|r| r.status == RegressionStatus::Warning)
            .count();
        let no_baseline_count = results
            .iter()
            .filter(|r| r.baseline_ratio.is_none())
            .count();
        let inconclusive_count = results
            .iter()
            .filter(|r| r.status == RegressionStatus::Inconclusive || r.baseline_ratio.is_none())
            .count();
        let ok_count = results
            .iter()
            .filter(|r| r.status == RegressionStatus::Ok && r.baseline_ratio.is_some())
            .count();

        // Warning-only mode can tolerate a measured regression, never missing evidence.
        let passed = total_operations > 0
            && inconclusive_count == 0
            && (!config.strict_mode || (regression_count == 0 && warning_count == 0));

        Self {
            total_operations,
            regression_count,
            warning_count,
            ok_count,
            no_baseline_count,
            inconclusive_count,
            results,
            passed,
            config_summary: RegressionConfigSummary::from(config),
        }
    }

    /// Print a human-readable summary table.
    pub fn print_table(&self) {
        println!("\n{}", "=".repeat(80));
        println!("REGRESSION CHECK SUMMARY");
        println!("{}", "=".repeat(80));

        println!(
            "Config: duration_threshold={:.0}%, rss_threshold={:.0}%, strict_mode={}",
            (self.config_summary.duration_threshold - 1.0) * 100.0,
            (self.config_summary.rss_threshold - 1.0) * 100.0,
            self.config_summary.strict_mode
        );
        println!();

        if self.no_baseline_count == self.total_operations {
            println!(
                "INCONCLUSIVE: no usable comparisons. Baseline capture is not a regression pass."
            );
            return;
        }

        println!(
            "{:<25} {:<15} {:>12} {:>12} {:>12} Reason",
            "Dataset/Operation", "Status", "Current", "Baseline", "Change"
        );
        println!("{}", "-".repeat(95));

        for result in &self.results {
            let key = format!("{}/{}", result.dataset, result.operation);
            let status = format!("{}", result.status);
            let current = format!("{:.3}", result.current_ratio);
            let baseline = result
                .baseline_ratio
                .map_or_else(|| "n/a".to_string(), |r| format!("{:.3}", r));
            let change = result
                .change_pct
                .map_or_else(|| "n/a".to_string(), |p| format!("{:+.1}%", p));

            // Truncate reason for display
            let reason = if result.reason.len() > 30 {
                format!("{}...", &result.reason[..27])
            } else {
                result.reason.clone()
            };

            println!("{key:<25} {status:<15} {current:>12} {baseline:>12} {change:>12} {reason}");
        }

        println!("{}", "-".repeat(95));
        println!(
            "Total: {} ops | {} ok | {} no baseline | {} inconclusive | {} regressions | Passed: {}",
            self.total_operations,
            self.ok_count,
            self.no_baseline_count,
            self.inconclusive_count,
            self.regression_count,
            if self.passed { "YES" } else { "NO" }
        );
    }
}

/// Required provenance and workload dimensions for raw release measurements.
pub const MATCHED_RUN_METADATA: [&str; 17] = [
    "command",
    "issue_count",
    "dataset_sha256",
    "flush_mode",
    "cache_protocol",
    "host",
    "host_boot_id",
    "cpu",
    "os",
    "filesystem",
    "target",
    "features",
    "engine",
    "source_revision",
    "lockfile_sha256",
    "binary_sha256",
    "build_profile",
];

/// Coverage is conditional on IID whole blocks, not independent invocations.
/// A quiet-runner admission check does not establish this statistical assumption.
pub const MATCHED_BLOCK_PROTOCOL: &str = "abba_two_per_side_iid_blocks_assumed_v1";

/// A single side of an alternating baseline/candidate release measurement.
/// Timings include every measured invocation; failed invocations are retained.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchedRun {
    pub metadata: BTreeMap<String, String>,
    pub samples_ms: Vec<f64>,
    pub exit_codes: Vec<i32>,
    /// Actual retained ABBA block IDs, aligned with timings and exit codes.
    /// Absent block evidence permits descriptive summaries only.
    #[serde(default)]
    pub block_ids: Vec<usize>,
}

impl MatchedRun {
    /// Read and validate a receipt. Missing/corrupt receipts never become defaults.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let run: Self = serde_json::from_str(&fs::read_to_string(path)?)?;
        run.validate()
            .map_err(|reason| std::io::Error::new(std::io::ErrorKind::InvalidData, reason))?;
        Ok(run)
    }

    pub fn validate(&self) -> Result<(), String> {
        for key in MATCHED_RUN_METADATA {
            let Some(value) = self.metadata.get(key) else {
                return Err(format!("missing metadata: {key}"));
            };
            if value.trim().is_empty()
                || matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "unknown" | "default" | "unavailable" | "n/a" | "placeholder" | "unset"
                )
            {
                return Err(format!("unusable metadata: {key}"));
            }
        }
        for key in ["dataset_sha256", "lockfile_sha256", "binary_sha256"] {
            let digest = &self.metadata[key];
            if digest.len() != 64
                || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                || digest.bytes().all(|byte| byte == b'0')
            {
                return Err(format!("invalid SHA-256 provenance: {key}"));
            }
        }
        if self.metadata["issue_count"].parse::<usize>().is_err() {
            return Err("invalid issue_count metadata".to_string());
        }
        if self.metadata["build_profile"] != "release" {
            return Err("build_profile must be release".to_string());
        }
        if self.samples_ms.len() < 20 {
            return Err(format!(
                "insufficient samples: {} (at least 20 required)",
                self.samples_ms.len()
            ));
        }
        if self.samples_ms.len() != self.exit_codes.len() {
            return Err("each sample must have an exit code".to_string());
        }
        if let Some(index) = self.samples_ms.iter().position(|&ms| !positive_finite(ms)) {
            return Err(format!("sample {index} must be positive and finite"));
        }
        if let Some(index) = self.exit_codes.iter().position(|&code| code != 0) {
            return Err(format!(
                "sample {index} failed with exit code {}",
                self.exit_codes[index]
            ));
        }
        Ok(())
    }

    fn validate_blocks(&self) -> Result<(), String> {
        if self.metadata.get("sampling_protocol").map(String::as_str)
            != Some(MATCHED_BLOCK_PROTOCOL)
        {
            return Err("missing or unsupported sampling_protocol for quantile inference".into());
        }
        if self.block_ids.len() != self.samples_ms.len()
            || !self.block_ids.len().is_multiple_of(2)
            || self
                .block_ids
                .as_chunks::<2>()
                .0
                .iter()
                .any(|pair| pair[0] != pair[1])
            || self
                .block_ids
                .windows(4)
                .step_by(2)
                .any(|window| window[0].checked_add(1) != Some(window[2]))
        {
            return Err(
                "block_ids must identify contiguous blocks with exactly two samples each".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchedState {
    Pass,
    Regression,
    Inconclusive,
}

/// Exact empirical quantile change, in milliseconds and percent of baseline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimingDelta {
    pub baseline_ms: f64,
    pub candidate_ms: f64,
    pub delta_ms: f64,
    pub delta_pct: f64,
}

impl TimingDelta {
    fn new(baseline_ms: f64, candidate_ms: f64) -> Self {
        Self {
            baseline_ms,
            candidate_ms,
            delta_ms: candidate_ms - baseline_ms,
            delta_pct: (candidate_ms / baseline_ms - 1.0) * 100.0,
        }
    }
}

/// Conservative observed-support bounds: [candidate_min - baseline_max,
/// candidate_max - baseline_min], with analogous relative bounds. These enclose
/// all median/p95 changes obtainable by resampling the observed values. They
/// are NOT a population confidence interval and assume no distribution model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedSupportInterval {
    pub method: String,
    pub lower_ms: f64,
    pub upper_ms: f64,
    /// None means the descriptive percentage is outside the finite f64 range.
    pub lower_pct: Option<f64>,
    pub upper_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchedComparison {
    pub state: MatchedState,
    pub command: String,
    pub budget_pct: Option<f64>,
    pub median: Option<TimingDelta>,
    pub p95: Option<TimingDelta>,
    pub observed_support: Option<ObservedSupportInterval>,
    pub uncertainty: Option<QuantileUncertainty>,
    pub diagnostic: String,
}

/// One-based order-statistic ranks; 0 and block_count+1 mean unbounded endpoints.
/// JSON nulls preserve unavailable bounds instead of clamping them to extrema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuantileInterval {
    pub lower_rank: usize,
    pub upper_rank: usize,
    pub baseline_lower_ms: f64,
    pub baseline_upper_ms: Option<f64>,
    pub candidate_lower_ms: f64,
    pub candidate_upper_ms: Option<f64>,
    pub lower: Option<TimingDelta>,
    pub upper: Option<TimingDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuantileUncertainty {
    pub method: String,
    pub assumption: String,
    pub coverage_scope: String,
    pub confidence_level: f64,
    pub one_sided_error_probability: f64,
    pub block_count: usize,
    pub median: QuantileInterval,
    pub p95: QuantileInterval,
}

impl MatchedComparison {
    pub const fn exit_code(&self) -> i32 {
        match self.state {
            MatchedState::Pass => 0,
            MatchedState::Regression => 1,
            MatchedState::Inconclusive => 2,
        }
    }
}

fn positive_finite(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

/// Empirical raw-sample summary; this does not make a budget or gate decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchedSampleSummary {
    pub sample_count: usize,
    pub median_ms: f64,
    pub p95_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
}

/// Summarize every positive finite sample, without filtering. Median averages
/// the middle two values for even counts; p95 is nearest rank. Fewer than 20
/// samples can be summarized, but cannot pass `compare_matched_runs`.
pub fn summarize_matched_samples(samples: &[f64]) -> Result<MatchedSampleSummary, String> {
    if samples.is_empty() || samples.iter().any(|&sample| !positive_finite(sample)) {
        return Err("samples must be nonempty, positive and finite".to_string());
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let count = sorted.len();
    let middle = count / 2;
    let median_ms = if count.is_multiple_of(2) {
        sorted[middle - 1] + (sorted[middle] - sorted[middle - 1]) / 2.0
    } else {
        sorted[middle]
    };
    // ceil(19*n/20)-1, without multiplying the count or casting to floats.
    let p95_index = count - count / 20 - 1;
    Ok(MatchedSampleSummary {
        sample_count: count,
        median_ms,
        p95_ms: sorted[p95_index],
        min_ms: sorted[0],
        max_ms: sorted[count - 1],
    })
}

fn validate_and_summarize_matched_runs(
    baseline: Option<&MatchedRun>,
    candidate: &MatchedRun,
) -> Result<(MatchedSampleSummary, MatchedSampleSummary), String> {
    candidate
        .validate()
        .map_err(|error| format!("candidate: {error}"))?;
    let baseline = baseline.ok_or_else(|| "missing baseline receipt".to_string())?;
    baseline
        .validate()
        .map_err(|error| format!("baseline: {error}"))?;
    if baseline.samples_ms.len() != candidate.samples_ms.len() {
        return Err("baseline/candidate sample counts differ".to_string());
    }
    for key in baseline.metadata.keys().chain(candidate.metadata.keys()) {
        if !matches!(
            key.as_str(),
            "source_revision" | "lockfile_sha256" | "binary_sha256"
        ) && baseline.metadata.get(key) != candidate.metadata.get(key)
        {
            return Err(format!("mismatched metadata: {key}"));
        }
    }
    Ok((
        summarize_matched_samples(&baseline.samples_ms)?,
        summarize_matched_samples(&candidate.samples_ms)?,
    ))
}

const QUANTILE_TAIL_ALPHA: f64 = 1.0 / 160.0;

/// Exact binomial order-statistic construction (NIST TN 2119, section 5.3).
/// For K~Bin(n,p), choose largest r with P(K<r)<=alpha and smallest s with
/// P(K>=s)<=alpha. Ranks 0/n+1 represent genuinely unbounded endpoints.
/// Mode-centered relative masses avoid underflow at p=.95 for large n.
fn quantile_ranks(
    blocks: usize,
    numerator: u32,
    denominator: u32,
) -> Result<(usize, usize), String> {
    let trials = u32::try_from(blocks).map_err(|_| "too many blocks for quantile inference")?;
    let mode =
        u32::try_from((u64::from(trials) + 1) * u64::from(numerator) / u64::from(denominator))
            .map_err(|_| "binomial mode overflow")?;
    let mode_index = usize::try_from(mode).map_err(|_| "binomial mode exceeds index range")?;
    let n = f64::from(trials);
    let odds = f64::from(numerator) / f64::from(denominator - numerator);
    let mut masses = vec![0.0; blocks + 1];
    masses[mode_index] = 1.0;
    let mut k = f64::from(mode);
    for index in (1..=mode_index).rev() {
        masses[index - 1] = masses[index] * k / (n - k + 1.0) / odds;
        k -= 1.0;
    }
    k = f64::from(mode);
    for index in mode_index..blocks {
        masses[index + 1] = masses[index] * (n - k) / (k + 1.0) * odds;
        k += 1.0;
    }
    let total: f64 = masses.iter().sum();
    // Widen, never narrow, the interval when a tail is near the floating-point
    // comparison boundary. Accumulate each tail directly, never as 1-CDF.
    let alpha = (32.0 * f64::EPSILON).mul_add(-(n + 1.0), QUANTILE_TAIL_ALPHA);
    if !positive_finite(total) || alpha <= 0.0 {
        return Err("insufficient numerical precision for binomial tails".into());
    }
    let mut lower = 0;
    let mut tail = 0.0;
    for (index, mass) in masses.iter().take(blocks).enumerate() {
        tail += mass / total;
        if tail > alpha {
            break;
        }
        lower = index + 1;
    }
    let mut upper = blocks + 1;
    tail = 0.0;
    for (index, mass) in masses.iter().enumerate().skip(1).rev() {
        tail += mass / total;
        if tail > alpha {
            break;
        }
        upper = index;
    }
    Ok((lower, upper))
}

fn block_extrema(run: &MatchedRun) -> (Vec<f64>, Vec<f64>) {
    let (mut minima, mut maxima): (Vec<_>, Vec<_>) = run
        .samples_ms
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| (pair[0].min(pair[1]), pair[0].max(pair[1])))
        .unzip();
    minima.sort_by(f64::total_cmp);
    maxima.sort_by(f64::total_cmp);
    (minima, maxima)
}

fn quantile_interval(
    baseline: &(Vec<f64>, Vec<f64>),
    candidate: &(Vec<f64>, Vec<f64>),
    numerator: u32,
    denominator: u32,
) -> Result<QuantileInterval, String> {
    let (lower_rank, upper_rank) = quantile_ranks(baseline.0.len(), numerator, denominator)?;
    let baseline_lower_ms = lower_rank.checked_sub(1).map_or(0.0, |i| baseline.0[i]);
    let candidate_lower_ms = lower_rank.checked_sub(1).map_or(0.0, |i| candidate.0[i]);
    let baseline_upper_ms = baseline.1.get(upper_rank - 1).copied();
    let candidate_upper_ms = candidate.1.get(upper_rank - 1).copied();
    let lower = baseline_upper_ms.map(|ms| TimingDelta::new(ms, candidate_lower_ms));
    let upper = candidate_upper_ms
        .filter(|_| baseline_lower_ms > 0.0)
        .map(|ms| TimingDelta::new(baseline_lower_ms, ms));
    if lower
        .iter()
        .chain(upper.iter())
        .any(|delta| !delta.delta_ms.is_finite() || !delta.delta_pct.is_finite())
    {
        return Err("quantile comparison arithmetic overflow".into());
    }
    Ok(QuantileInterval {
        lower_rank,
        upper_rank,
        baseline_lower_ms,
        baseline_upper_ms,
        candidate_lower_ms,
        candidate_upper_ms,
        lower,
        upper,
    })
}

fn infer_quantiles(
    baseline: &MatchedRun,
    candidate: &MatchedRun,
) -> Result<QuantileUncertainty, String> {
    baseline.validate_blocks()?;
    candidate.validate_blocks()?;
    if baseline.block_ids != candidate.block_ids {
        return Err("mismatched baseline/candidate block_ids".into());
    }
    let baseline_extrema = block_extrema(baseline);
    let candidate_extrema = block_extrema(candidate);
    Ok(QuantileUncertainty {
        method: "binomial_order_statistics_of_block_minima_and_maxima".into(),
        assumption: "independent identically distributed whole ABBA blocks; dependence within a block allowed; runner load does not establish this assumption".into(),
        coverage_scope: "joint median and p95 for this comparison only; not simultaneous across workloads or repeated comparisons".into(),
        confidence_level: 0.95,
        one_sided_error_probability: QUANTILE_TAIL_ALPHA,
        block_count: baseline_extrema.0.len(),
        median: quantile_interval(&baseline_extrema, &candidate_extrema, 1, 2)?,
        p95: quantile_interval(&baseline_extrema, &candidate_extrema, 19, 20)?,
    })
}

impl QuantileUncertainty {
    fn classify(&self, budget_pct: f64) -> MatchedState {
        if !budget_pct.is_finite() || budget_pct < 0.0 {
            return MatchedState::Inconclusive;
        }
        let intervals = [&self.median, &self.p95];
        let budget_ms = |delta: &TimingDelta| delta.baseline_ms * (budget_pct / 100.0);
        if intervals.iter().any(|interval| {
            interval.lower.as_ref().is_some_and(|delta| {
                budget_ms(delta).is_finite() && delta.delta_ms > budget_ms(delta)
            })
        }) {
            MatchedState::Regression
        } else if intervals.iter().all(|interval| {
            interval.upper.as_ref().is_some_and(|delta| {
                budget_ms(delta).is_finite() && delta.delta_ms <= budget_ms(delta)
            })
        }) {
            MatchedState::Pass
        } else {
            MatchedState::Inconclusive
        }
    }
}

/// Compare raw timings without filtering outliers. Median averages the middle
/// two values for even counts; p95 uses nearest rank (ceil(0.95*n), one-based).
/// Source/lockfile/binary provenance may differ intentionally; all other metadata
/// must match. Conditional IID-block quantile bounds decide the gate: both
/// upper bounds must meet budget to pass; either lower bound can prove a
/// regression. Extrema remain descriptive and never substitute for uncertainty.
/// An invalid/absent budget retains descriptive deltas but cannot pass the gate.
pub fn compare_matched_runs(
    baseline: Option<&MatchedRun>,
    candidate: &MatchedRun,
    budget_pct: f64,
) -> MatchedComparison {
    let command = candidate
        .metadata
        .get("command")
        .cloned()
        .unwrap_or_else(|| "<missing command>".to_string());
    let valid_budget = budget_pct.is_finite() && budget_pct >= 0.0;
    let budget_description = if valid_budget {
        format!("{budget_pct:.3}%")
    } else {
        "unavailable (requires a finite nonnegative percentage)".to_string()
    };
    let mut comparison = MatchedComparison {
        state: MatchedState::Inconclusive,
        diagnostic: format!("{command}: budget {budget_description}; delta unavailable"),
        command,
        budget_pct: valid_budget.then_some(budget_pct),
        median: None,
        p95: None,
        observed_support: None,
        uncertainty: None,
    };
    let reason = validate_and_summarize_matched_runs(baseline, candidate);
    let (baseline_summary, candidate_summary) = match reason {
        Ok(summaries) => summaries,
        Err(reason) => {
            comparison
                .diagnostic
                .push_str(&format!("; inconclusive: {reason}"));
            return comparison;
        }
    };

    let median = TimingDelta::new(baseline_summary.median_ms, candidate_summary.median_ms);
    let p95 = TimingDelta::new(baseline_summary.p95_ms, candidate_summary.p95_ms);
    let lower = TimingDelta::new(baseline_summary.max_ms, candidate_summary.min_ms);
    let upper = TimingDelta::new(baseline_summary.min_ms, candidate_summary.max_ms);
    if [&median, &p95]
        .iter()
        .any(|delta| !delta.delta_ms.is_finite() || !delta.delta_pct.is_finite())
    {
        comparison
            .diagnostic
            .push_str("; inconclusive: comparison arithmetic overflow");
        return comparison;
    }
    let inference = infer_quantiles(baseline.expect("validated baseline receipt"), candidate);
    let inference_diagnostic = match inference {
        Ok(uncertainty) => {
            comparison.state = uncertainty.classify(budget_pct);
            let diagnostic = format!(
                "conditional IID-block 95% joint median/p95 bounds for this comparison only; {} blocks; median ranks [{}, {}], p95 ranks [{}, {}]; unbounded endpoints remain null",
                uncertainty.block_count,
                uncertainty.median.lower_rank,
                uncertainty.median.upper_rank,
                uncertainty.p95.lower_rank,
                uncertainty.p95.upper_rank,
            );
            comparison.uncertainty = Some(uncertainty);
            diagnostic
        }
        Err(reason) => format!("quantile inference unavailable: {reason}"),
    };
    let support_percentage = |pct: f64| {
        if pct.is_finite() {
            format!("{pct:+.3}%")
        } else {
            "unrepresentable".into()
        }
    };
    comparison.diagnostic = format!(
        "{}: {:?}; budget {}; median delta {:+.6} ms ({:+.3}%); p95 delta {:+.6} ms ({:+.3}%); observed-support range [{}, {}] (descriptive only); {}",
        comparison.command,
        comparison.state,
        budget_description,
        median.delta_ms,
        median.delta_pct,
        p95.delta_ms,
        p95.delta_pct,
        support_percentage(lower.delta_pct),
        support_percentage(upper.delta_pct),
        inference_diagnostic,
    );
    comparison.median = Some(median);
    comparison.p95 = Some(p95);
    comparison.observed_support = Some(ObservedSupportInterval {
        method: "observed_support_extrema_not_confidence_interval".to_string(),
        lower_ms: lower.delta_ms,
        upper_ms: upper.delta_ms,
        lower_pct: lower.delta_pct.is_finite().then_some(lower.delta_pct),
        upper_pct: upper.delta_pct.is_finite().then_some(upper.delta_pct),
    });
    comparison
}

// =============================================================================
// Baseline Update Helper
// =============================================================================

/// Helper to update baselines from benchmark results.
pub fn update_baselines_from_results(
    store: &mut BaselineStore,
    dataset_name: &str,
    issue_count: usize,
    comparisons: &[(String, f64, u128, u128, Option<f64>)], // (label, ratio, br_ms, bd_ms, rss_ratio)
) {
    let timestamp = chrono::Utc::now().to_rfc3339();

    for (label, ratio, br_ms, bd_ms, rss_ratio) in comparisons {
        store.set_baseline(
            dataset_name,
            issue_count,
            label,
            OperationBaseline {
                duration_ratio: *ratio,
                rss_ratio: *rss_ratio,
                br_duration_ms: *br_ms,
                bd_duration_ms: *bd_ms,
                captured_at: timestamp.clone(),
                notes: None,
            },
        );
    }
}

/// Check if baseline update is requested via environment.
pub fn should_update_baseline() -> bool {
    env::var("BENCH_UPDATE_BASELINE").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regression_config_defaults() {
        let config = RegressionConfig::default();
        assert!((config.duration_threshold - 1.20).abs() < 0.001);
        assert!((config.rss_threshold - 1.30).abs() < 0.001);
        assert!(!config.strict_mode);
    }

    #[test]
    fn test_regression_check_no_baseline() {
        let result = RegressionResult::no_baseline("list", "beads_rust", 0.5, None);
        assert!(!result.is_regression);
        assert_eq!(result.status, RegressionStatus::Inconclusive);
        assert!(result.baseline_ratio.is_none());
    }

    #[test]
    fn test_regression_check_improvement() {
        let config = RegressionConfig::default();
        let baseline = OperationBaseline {
            duration_ratio: 0.5,
            rss_ratio: None,
            br_duration_ms: 100,
            bd_duration_ms: 200,
            captured_at: "2026-01-01".to_string(),
            notes: None,
        };

        // Current is 0.4 (better than baseline 0.5)
        let result = RegressionResult::check("list", "beads_rust", 0.4, None, &baseline, &config);
        assert!(!result.is_regression);
        assert_eq!(result.status, RegressionStatus::Ok);
        assert!(result.reason.contains("faster"));
    }

    #[test]
    fn test_regression_check_within_threshold() {
        let config = RegressionConfig::default();
        let baseline = OperationBaseline {
            duration_ratio: 0.5,
            rss_ratio: None,
            br_duration_ms: 100,
            bd_duration_ms: 200,
            captured_at: "2026-01-01".to_string(),
            notes: None,
        };

        // Current is 0.55 (10% worse than baseline 0.5, within 20% threshold)
        let result = RegressionResult::check("list", "beads_rust", 0.55, None, &baseline, &config);
        assert!(!result.is_regression);
        assert_eq!(result.status, RegressionStatus::Ok);
    }

    #[test]
    fn test_regression_check_exceeds_threshold() {
        let config = RegressionConfig::default();
        let baseline = OperationBaseline {
            duration_ratio: 0.5,
            rss_ratio: None,
            br_duration_ms: 100,
            bd_duration_ms: 200,
            captured_at: "2026-01-01".to_string(),
            notes: None,
        };

        // Current is 0.7 (40% worse than baseline 0.5, exceeds 20% threshold)
        let result = RegressionResult::check("list", "beads_rust", 0.7, None, &baseline, &config);
        assert!(result.is_regression);
        assert_eq!(result.status, RegressionStatus::Regression);
    }

    #[test]
    fn test_baseline_store_roundtrip() {
        let mut store = BaselineStore::default();
        store.set_baseline(
            "test_dataset",
            100,
            "list",
            OperationBaseline {
                duration_ratio: 0.5,
                rss_ratio: Some(0.8),
                br_duration_ms: 100,
                bd_duration_ms: 200,
                captured_at: "2026-01-01".to_string(),
                notes: Some("Test baseline".to_string()),
            },
        );

        let json = serde_json::to_string_pretty(&store).unwrap();
        let loaded: BaselineStore = serde_json::from_str(&json).unwrap();

        let baseline = loaded.get_baseline("test_dataset", "list").unwrap();
        assert!((baseline.duration_ratio - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_regression_summary() {
        let config = RegressionConfig::default();
        let results = vec![
            RegressionResult::no_baseline("list", "ds1", 0.5, None),
            RegressionResult {
                operation: "ready".to_string(),
                dataset: "ds1".to_string(),
                is_regression: false,
                status: RegressionStatus::Ok,
                current_ratio: 0.4,
                baseline_ratio: Some(0.5),
                change_pct: Some(-20.0),
                current_rss_ratio: None,
                baseline_rss_ratio: None,
                rss_change_pct: None,
                reason: "Improved".to_string(),
            },
            RegressionResult {
                operation: "stats".to_string(),
                dataset: "ds1".to_string(),
                is_regression: true,
                status: RegressionStatus::Regression,
                current_ratio: 0.8,
                baseline_ratio: Some(0.5),
                change_pct: Some(60.0),
                current_rss_ratio: None,
                baseline_rss_ratio: None,
                rss_change_pct: None,
                reason: "60% slower".to_string(),
            },
        ];

        let summary = RegressionSummary::from_results(results, &config);
        assert_eq!(summary.total_operations, 3);
        assert_eq!(summary.no_baseline_count, 1);
        assert_eq!(summary.ok_count, 1);
        assert_eq!(summary.regression_count, 1);
        assert_eq!(summary.inconclusive_count, 1);
        assert!(!summary.passed); // Missing evidence cannot pass even in warning-only mode.
    }

    #[test]
    fn test_regression_summary_strict_mode() {
        let config = RegressionConfig {
            strict_mode: true,
            ..Default::default()
        };

        let results = vec![RegressionResult {
            operation: "list".to_string(),
            dataset: "ds1".to_string(),
            is_regression: true,
            status: RegressionStatus::Regression,
            current_ratio: 0.8,
            baseline_ratio: Some(0.5),
            change_pct: Some(60.0),
            current_rss_ratio: None,
            baseline_rss_ratio: None,
            rss_change_pct: None,
            reason: "Regression".to_string(),
        }];

        let summary = RegressionSummary::from_results(results, &config);
        assert!(!summary.passed); // Strict mode fails on regression
    }
}
