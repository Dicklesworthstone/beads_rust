//! Cold vs Warm Start Benchmarks: Startup and Cache Effect Analysis
//!
//! This module measures cold start (first process invocation) vs warm start (repeated runs)
//! performance for both br (Rust) and bd (Go) implementations.
//!
//! # What is Cold vs Warm?
//!
//! - **Cold start**: First process invocation after process termination.
//!   The binary must be loaded from disk, `SQLite` must initialize, and no
//!   filesystem cache benefits are expected (though OS may cache).
//!
//! - **Warm start**: Subsequent process invocations. The OS has likely cached
//!   the binary, `SQLite` pages may be cached, and filesystem metadata is warm.
//!
//! # Measured Commands
//!
//! - `list --json` - List all issues (heavy DB read)
//! - `ready --json` - Get ready issues (dependency resolution)
//! - `stats --json` - Project statistics (aggregation)
//! - `sync --status` - Check sync status (lightweight read)
//!
//! # Usage
//!
//! ```bash
//! # Run cold/warm benchmarks (requires bd installed)
//! cargo test --test bench_cold_warm -- --ignored --nocapture
//!
//! # Quick run on beads_rust dataset only
//! cargo test --test bench_cold_warm cold_warm_quick -- --ignored --nocapture
//!
//! # With filesystem cache drop (requires sudo)
//! BENCH_DROP_CACHES=1 sudo -E cargo test --test bench_cold_warm -- --ignored --nocapture
//! ```
//!
//! # Output
//!
//! Results are written to `target/benchmark-results/cold_warm_*.json` with
//! explicit `cold` and `warm` tags in each measurement.

#![allow(clippy::cast_precision_loss, clippy::similar_names)]

mod common;

use common::{
    DatasetIntegrityGuard, DiscoveredBinaries, IsolatedDataset, KnownDataset, discover_binaries,
    init_test_logging,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// =============================================================================
// Configuration
// =============================================================================

/// Benchmark configuration for cold/warm measurements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdWarmConfig {
    /// Number of cold start measurements (each requires fresh process)
    pub cold_runs: usize,
    /// Number of warm start measurements after warmup
    pub warm_runs: usize,
    /// Number of warmup runs before warm measurements (discarded)
    pub warmup_runs: usize,
    /// Whether to attempt dropping filesystem caches between cold runs
    /// (requires elevated privileges on Linux)
    pub drop_caches: bool,
    /// Delay between cold runs in milliseconds (allows OS cleanup)
    pub cold_run_delay_ms: u64,
}

impl Default for ColdWarmConfig {
    fn default() -> Self {
        Self {
            cold_runs: 3,
            warm_runs: 5,
            warmup_runs: 2,
            drop_caches: std::env::var("BENCH_DROP_CACHES").is_ok(),
            cold_run_delay_ms: 100,
        }
    }
}

impl ColdWarmConfig {
    /// Quick config for CI (fewer runs)
    #[must_use]
    pub const fn quick() -> Self {
        Self {
            cold_runs: 2,
            warm_runs: 3,
            warmup_runs: 1,
            drop_caches: false,
            cold_run_delay_ms: 50,
        }
    }
}

// =============================================================================
// Metrics
// =============================================================================

/// Single measurement with timing and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurement {
    /// "cold" or "warm"
    pub start_type: String,
    /// Which binary ("br" or "bd")
    pub binary: String,
    /// Command label (e.g., "list", "ready")
    pub command: String,
    /// Run index within this start type
    pub run_index: usize,
    /// Duration in milliseconds
    pub duration_ms: f64,
    /// Exit code
    pub exit_code: i32,
    /// Whether command succeeded
    pub success: bool,
    /// Stdout size in bytes
    pub stdout_bytes: usize,
}

/// Statistics for a series of measurements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimingStats {
    pub mean_ms: f64,
    pub median_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub stddev_ms: f64,
    pub count: usize,
}

impl TimingStats {
    #[must_use]
    pub fn from_measurements(measurements: &[Measurement]) -> Self {
        if measurements.is_empty() {
            return Self {
                mean_ms: 0.0,
                median_ms: 0.0,
                min_ms: 0.0,
                max_ms: 0.0,
                stddev_ms: 0.0,
                count: 0,
            };
        }

        let mut values: Vec<f64> = measurements.iter().map(|m| m.duration_ms).collect();
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let n = values.len();
        let mean = values.iter().sum::<f64>() / n as f64;
        let median = if n % 2 == 0 {
            f64::midpoint(values[n / 2 - 1], values[n / 2])
        } else {
            values[n / 2]
        };
        let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
        let stddev = variance.sqrt();

        Self {
            mean_ms: mean,
            median_ms: median,
            min_ms: values[0],
            max_ms: values[n - 1],
            stddev_ms: stddev,
            count: n,
        }
    }
}

/// Cold vs warm comparison for a single command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdWarmComparison {
    /// Command being benchmarked
    pub command: String,
    /// Cold start statistics
    pub cold_stats: TimingStats,
    /// Warm start statistics
    pub warm_stats: TimingStats,
    /// Cold/warm ratio (>1 means cold is slower, as expected)
    pub cold_warm_ratio: f64,
    /// Startup overhead in ms (`cold_mean` - `warm_mean`)
    pub startup_overhead_ms: f64,
    /// Raw measurements for detailed analysis
    pub cold_measurements: Vec<Measurement>,
    pub warm_measurements: Vec<Measurement>,
}

/// Comparison between br and bd for cold/warm behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryComparison {
    pub command: String,
    pub br_cold_warm: ColdWarmComparison,
    pub bd_cold_warm: Option<ColdWarmComparison>,
    /// br cold / bd cold ratio (< 1 means br cold start is faster)
    pub cold_ratio: Option<f64>,
    /// br warm / bd warm ratio (< 1 means br warm start is faster)
    pub warm_ratio: Option<f64>,
    /// br overhead / bd overhead ratio
    pub overhead_ratio: Option<f64>,
}

/// Full benchmark results for a dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdWarmBenchmark {
    pub dataset_name: String,
    pub issue_count: usize,
    pub config: ColdWarmConfig,
    pub comparisons: Vec<BinaryComparison>,
    pub summary: ColdWarmSummary,
    pub timestamp: String,
}

/// Summary statistics across all commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdWarmSummary {
    /// Average cold/warm ratio for br
    pub br_avg_cold_warm_ratio: f64,
    /// Average cold/warm ratio for bd
    pub bd_avg_cold_warm_ratio: Option<f64>,
    /// Average startup overhead for br (ms)
    pub br_avg_overhead_ms: f64,
    /// Average startup overhead for bd (ms)
    pub bd_avg_overhead_ms: Option<f64>,
    /// Commands where br has lower cold start overhead
    pub br_faster_cold_count: usize,
    /// Commands where br has lower warm performance
    pub br_faster_warm_count: usize,
    /// Total commands benchmarked
    pub total_commands: usize,
}

// =============================================================================
// Cache Management
// =============================================================================

/// Attempt to drop filesystem caches (Linux only, requires root).
fn try_drop_caches() -> bool {
    #[cfg(target_os = "linux")]
    {
        // Try to drop page cache, dentries, and inodes
        if let Ok(status) = Command::new("sh")
            .args(["-c", "sync && echo 3 > /proc/sys/vm/drop_caches"])
            .status()
        {
            return status.success();
        }
        false
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Not supported on other platforms
        false
    }
}

// =============================================================================
// Command Execution
// =============================================================================

/// Run a command and capture timing.
fn run_timed(
    binary_path: &Path,
    args: &[&str],
    cwd: &Path,
    command_label: &str,
    binary_name: &str,
    start_type: &str,
    run_index: usize,
) -> Measurement {
    let start = Instant::now();

    let output = Command::new(binary_path)
        .args(args)
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("Failed to run command");

    let duration = start.elapsed();

    Measurement {
        start_type: start_type.to_string(),
        binary: binary_name.to_string(),
        command: command_label.to_string(),
        run_index,
        duration_ms: duration.as_secs_f64() * 1000.0,
        exit_code: output.status.code().unwrap_or(-1),
        success: output.status.success(),
        stdout_bytes: output.stdout.len(),
    }
}

/// Measure cold starts for a command.
fn measure_cold(
    binary_path: &Path,
    args: &[&str],
    cwd: &Path,
    command_label: &str,
    binary_name: &str,
    config: &ColdWarmConfig,
) -> Vec<Measurement> {
    let mut measurements = Vec::with_capacity(config.cold_runs);

    for i in 0..config.cold_runs {
        // Optionally drop caches between runs
        if config.drop_caches {
            try_drop_caches();
        }

        // Small delay to allow OS cleanup
        std::thread::sleep(Duration::from_millis(config.cold_run_delay_ms));

        let measurement = run_timed(
            binary_path,
            args,
            cwd,
            command_label,
            binary_name,
            "cold",
            i,
        );
        measurements.push(measurement);
    }

    measurements
}

/// Measure warm starts for a command.
fn measure_warm(
    binary_path: &Path,
    args: &[&str],
    cwd: &Path,
    command_label: &str,
    binary_name: &str,
    config: &ColdWarmConfig,
) -> Vec<Measurement> {
    // Warmup runs (discarded)
    for _ in 0..config.warmup_runs {
        let _ = run_timed(
            binary_path,
            args,
            cwd,
            command_label,
            binary_name,
            "warmup",
            0,
        );
    }

    // Timed warm runs
    let mut measurements = Vec::with_capacity(config.warm_runs);
    for i in 0..config.warm_runs {
        let measurement = run_timed(
            binary_path,
            args,
            cwd,
            command_label,
            binary_name,
            "warm",
            i,
        );
        measurements.push(measurement);
    }

    measurements
}

/// Measure cold and warm for a single binary and command.
fn measure_cold_warm(
    binary_path: &Path,
    args: &[&str],
    cwd: &Path,
    command_label: &str,
    binary_name: &str,
    config: &ColdWarmConfig,
) -> ColdWarmComparison {
    // Measure cold first (before any warm-up)
    let cold_measurements =
        measure_cold(binary_path, args, cwd, command_label, binary_name, config);

    // Then measure warm
    let warm_measurements =
        measure_warm(binary_path, args, cwd, command_label, binary_name, config);

    let cold_stats = TimingStats::from_measurements(&cold_measurements);
    let warm_stats = TimingStats::from_measurements(&warm_measurements);

    let cold_warm_ratio = if warm_stats.mean_ms > 0.0 {
        cold_stats.mean_ms / warm_stats.mean_ms
    } else {
        1.0
    };

    let startup_overhead_ms = cold_stats.mean_ms - warm_stats.mean_ms;

    ColdWarmComparison {
        command: command_label.to_string(),
        cold_stats,
        warm_stats,
        cold_warm_ratio,
        startup_overhead_ms,
        cold_measurements,
        warm_measurements,
    }
}

// =============================================================================
// Benchmark Runner
// =============================================================================

/// Commands to benchmark for cold/warm analysis.
const BENCHMARK_COMMANDS: &[(&str, &[&str])] = &[
    ("list", &["list", "--json"]),
    ("ready", &["ready", "--json"]),
    ("stats", &["stats", "--json"]),
    ("sync_status", &["sync", "--status"]),
];

/// Run cold/warm benchmarks for a dataset.
fn benchmark_dataset(
    dataset: KnownDataset,
    binaries: &DiscoveredBinaries,
    config: &ColdWarmConfig,
) -> Result<ColdWarmBenchmark, String> {
    // Create integrity guard
    let mut guard = DatasetIntegrityGuard::new(dataset)
        .map_err(|e| format!("Failed to create integrity guard: {e}"))?;

    let before = guard.verify_before();
    if !before.passed {
        return Err(format!("Source integrity check failed: {}", before.message));
    }

    // Create isolated copy
    let isolated = IsolatedDataset::from_dataset(dataset)
        .map_err(|e| format!("Failed to create workspace: {e}"))?;

    let dataset_name = dataset.name().to_string();
    let issue_count = isolated.metadata.issue_count;
    let workspace = isolated.workspace_root();

    println!("\nBenchmarking cold/warm: {dataset_name} ({issue_count} issues)");

    let bd_path = binaries.bd.as_ref().map(|b| b.path.clone());
    let mut comparisons = Vec::new();

    for (label, args) in BENCHMARK_COMMANDS {
        print!("  {label} ... ");

        // Measure br
        let br_cold_warm =
            measure_cold_warm(&binaries.br.path, args, workspace, label, "br", config);

        // Measure bd if available
        let bd_cold_warm = bd_path
            .as_ref()
            .map(|bd| measure_cold_warm(bd, args, workspace, label, "bd", config));

        // Calculate ratios
        let (cold_ratio, warm_ratio, overhead_ratio) = if let Some(ref bd_cw) = bd_cold_warm {
            let cr = if bd_cw.cold_stats.mean_ms > 0.0 {
                Some(br_cold_warm.cold_stats.mean_ms / bd_cw.cold_stats.mean_ms)
            } else {
                None
            };
            let wr = if bd_cw.warm_stats.mean_ms > 0.0 {
                Some(br_cold_warm.warm_stats.mean_ms / bd_cw.warm_stats.mean_ms)
            } else {
                None
            };
            let or = if bd_cw.startup_overhead_ms.abs() > 0.1 {
                Some(br_cold_warm.startup_overhead_ms / bd_cw.startup_overhead_ms)
            } else {
                None
            };
            (cr, wr, or)
        } else {
            (None, None, None)
        };

        let comparison = BinaryComparison {
            command: label.to_string(),
            br_cold_warm,
            bd_cold_warm,
            cold_ratio,
            warm_ratio,
            overhead_ratio,
        };

        println!(
            "cold: {:.1}ms, warm: {:.1}ms, overhead: {:.1}ms",
            comparison.br_cold_warm.cold_stats.mean_ms,
            comparison.br_cold_warm.warm_stats.mean_ms,
            comparison.br_cold_warm.startup_overhead_ms
        );

        comparisons.push(comparison);
    }

    // Verify source wasn't mutated
    let after = guard.verify_after();
    if !after.passed {
        return Err(format!(
            "Source dataset was mutated during benchmark: {}",
            after.message
        ));
    }

    // Calculate summary
    let summary = calculate_summary(&comparisons);

    Ok(ColdWarmBenchmark {
        dataset_name,
        issue_count,
        config: config.clone(),
        comparisons,
        summary,
        timestamp: chrono::Utc::now().to_rfc3339(),
    })
}

/// Calculate summary statistics from comparisons.
fn calculate_summary(comparisons: &[BinaryComparison]) -> ColdWarmSummary {
    let n = comparisons.len();
    if n == 0 {
        return ColdWarmSummary {
            br_avg_cold_warm_ratio: 1.0,
            bd_avg_cold_warm_ratio: None,
            br_avg_overhead_ms: 0.0,
            bd_avg_overhead_ms: None,
            br_faster_cold_count: 0,
            br_faster_warm_count: 0,
            total_commands: 0,
        };
    }

    let br_ratios: Vec<f64> = comparisons
        .iter()
        .map(|c| c.br_cold_warm.cold_warm_ratio)
        .collect();
    let br_avg_cold_warm_ratio = br_ratios.iter().sum::<f64>() / n as f64;

    let br_overheads: Vec<f64> = comparisons
        .iter()
        .map(|c| c.br_cold_warm.startup_overhead_ms)
        .collect();
    let br_avg_overhead_ms = br_overheads.iter().sum::<f64>() / n as f64;

    let bd_avg_cold_warm_ratio = {
        let bd_ratios: Vec<f64> = comparisons
            .iter()
            .filter_map(|c| c.bd_cold_warm.as_ref())
            .map(|cw| cw.cold_warm_ratio)
            .collect();
        if bd_ratios.is_empty() {
            None
        } else {
            Some(bd_ratios.iter().sum::<f64>() / bd_ratios.len() as f64)
        }
    };

    let bd_avg_overhead_ms = {
        let bd_overheads: Vec<f64> = comparisons
            .iter()
            .filter_map(|c| c.bd_cold_warm.as_ref())
            .map(|cw| cw.startup_overhead_ms)
            .collect();
        if bd_overheads.is_empty() {
            None
        } else {
            Some(bd_overheads.iter().sum::<f64>() / bd_overheads.len() as f64)
        }
    };

    let br_faster_cold_count = comparisons
        .iter()
        .filter(|c| c.cold_ratio.is_some_and(|r| r < 1.0))
        .count();

    let br_faster_warm_count = comparisons
        .iter()
        .filter(|c| c.warm_ratio.is_some_and(|r| r < 1.0))
        .count();

    ColdWarmSummary {
        br_avg_cold_warm_ratio,
        bd_avg_cold_warm_ratio,
        br_avg_overhead_ms,
        bd_avg_overhead_ms,
        br_faster_cold_count,
        br_faster_warm_count,
        total_commands: n,
    }
}

// =============================================================================
// Output
// =============================================================================

/// Print benchmark results table.
fn print_results(benchmark: &ColdWarmBenchmark) {
    let sep = "=".repeat(100);
    let dash = "-".repeat(100);

    println!("\n{sep}");
    println!(
        "Cold vs Warm Benchmark: {} ({} issues)",
        benchmark.dataset_name, benchmark.issue_count
    );
    println!("{sep}");

    println!(
        "\n{:<15} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "Command", "br cold", "br warm", "br ratio", "bd cold", "bd warm", "bd ratio"
    );
    println!("{dash}");

    for c in &benchmark.comparisons {
        let bd_cold = c.bd_cold_warm.as_ref().map_or_else(
            || "-".to_string(),
            |cw| format!("{:.1}", cw.cold_stats.mean_ms),
        );
        let bd_warm = c.bd_cold_warm.as_ref().map_or_else(
            || "-".to_string(),
            |cw| format!("{:.1}", cw.warm_stats.mean_ms),
        );
        let bd_ratio = c.bd_cold_warm.as_ref().map_or_else(
            || "-".to_string(),
            |cw| format!("{:.2}x", cw.cold_warm_ratio),
        );

        println!(
            "{:<15} {:>10.1} {:>10.1} {:>10.2}x {:>10} {:>10} {:>10}",
            c.command,
            c.br_cold_warm.cold_stats.mean_ms,
            c.br_cold_warm.warm_stats.mean_ms,
            c.br_cold_warm.cold_warm_ratio,
            bd_cold,
            bd_warm,
            bd_ratio
        );
    }

    println!("{dash}");

    // Summary
    println!("\nSUMMARY:");
    println!(
        "  br avg cold/warm ratio: {:.2}x, avg overhead: {:.1}ms",
        benchmark.summary.br_avg_cold_warm_ratio, benchmark.summary.br_avg_overhead_ms
    );
    if let (Some(bd_ratio), Some(bd_overhead)) = (
        benchmark.summary.bd_avg_cold_warm_ratio,
        benchmark.summary.bd_avg_overhead_ms,
    ) {
        println!("  bd avg cold/warm ratio: {bd_ratio:.2}x, avg overhead: {bd_overhead:.1}ms");
        println!(
            "  br faster on cold: {}/{}, br faster on warm: {}/{}",
            benchmark.summary.br_faster_cold_count,
            benchmark.summary.total_commands,
            benchmark.summary.br_faster_warm_count,
            benchmark.summary.total_commands
        );
    }
    println!();
}

/// Write results to JSON file.
fn write_results_json(benchmarks: &[ColdWarmBenchmark], output_path: &Path) -> std::io::Result<()> {
    let file = File::create(output_path)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer_pretty(writer, benchmarks)?;
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

/// Full cold/warm benchmark on all available datasets.
#[test]
#[ignore = "run with: cargo test --test bench_cold_warm -- --ignored --nocapture"]
fn cold_warm_full() {
    init_test_logging();

    println!("\n=== Cold vs Warm Start Benchmark Suite ===\n");

    let binaries = match discover_binaries() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Binary discovery failed: {e}");
            panic!("Cannot run benchmarks without br binary");
        }
    };

    println!(
        "br: {} ({})",
        binaries.br.path.display(),
        binaries.br.version
    );
    if let Some(ref bd) = binaries.bd {
        println!("bd: {} ({})", bd.path.display(), bd.version);
    } else {
        println!("bd: NOT FOUND - will benchmark br only");
    }

    let config = ColdWarmConfig::default();
    println!(
        "\nConfig: {} cold runs, {} warm runs, {} warmup runs",
        config.cold_runs, config.warm_runs, config.warmup_runs
    );
    if config.drop_caches {
        println!("  Cache dropping: ENABLED");
    }

    let mut results: Vec<ColdWarmBenchmark> = Vec::new();

    for dataset in KnownDataset::all() {
        if !dataset.beads_dir().exists() {
            println!("\nSkipping {} (not available)", dataset.name());
            continue;
        }

        match benchmark_dataset(*dataset, &binaries, &config) {
            Ok(benchmark) => {
                print_results(&benchmark);
                results.push(benchmark);
            }
            Err(e) => {
                eprintln!("\nFailed to benchmark {}: {}", dataset.name(), e);
            }
        }
    }

    // Write results
    if !results.is_empty() {
        let output_dir = PathBuf::from("target/benchmark-results");
        fs::create_dir_all(&output_dir).expect("create output dir");

        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let output_path = output_dir.join(format!("cold_warm_{timestamp}.json"));

        if let Err(e) = write_results_json(&results, &output_path) {
            eprintln!("Failed to write results: {e}");
        } else {
            println!("\nResults written to: {}", output_path.display());
        }

        // Also write latest
        let latest_path = output_dir.join("cold_warm_latest.json");
        let _ = write_results_json(&results, &latest_path);
    }
}

/// Quick cold/warm benchmark on `beads_rust` only.
#[test]
#[ignore = "run with: cargo test --test bench_cold_warm cold_warm_quick -- --ignored --nocapture"]
fn cold_warm_quick() {
    init_test_logging();

    println!("\n=== Quick Cold vs Warm Benchmark ===\n");

    let binaries = match discover_binaries() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Binary discovery failed: {e}");
            panic!("Cannot run benchmarks without br binary");
        }
    };

    println!("br: {}", binaries.br.path.display());
    if let Some(ref bd) = binaries.bd {
        println!("bd: {}", bd.path.display());
    }

    let config = ColdWarmConfig::quick();

    match benchmark_dataset(KnownDataset::BeadsRust, &binaries, &config) {
        Ok(benchmark) => {
            print_results(&benchmark);
        }
        Err(e) => {
            panic!("Failed to benchmark beads_rust: {e}");
        }
    }
}

/// Unit test for timing stats calculation.
#[test]
fn test_timing_stats() {
    let measurements = vec![
        Measurement {
            start_type: "warm".to_string(),
            binary: "br".to_string(),
            command: "list".to_string(),
            run_index: 0,
            duration_ms: 10.0,
            exit_code: 0,
            success: true,
            stdout_bytes: 100,
        },
        Measurement {
            start_type: "warm".to_string(),
            binary: "br".to_string(),
            command: "list".to_string(),
            run_index: 1,
            duration_ms: 20.0,
            exit_code: 0,
            success: true,
            stdout_bytes: 100,
        },
        Measurement {
            start_type: "warm".to_string(),
            binary: "br".to_string(),
            command: "list".to_string(),
            run_index: 2,
            duration_ms: 30.0,
            exit_code: 0,
            success: true,
            stdout_bytes: 100,
        },
    ];

    let stats = TimingStats::from_measurements(&measurements);

    assert!((stats.mean_ms - 20.0).abs() < 0.01);
    assert!((stats.median_ms - 20.0).abs() < 0.01);
    assert!((stats.min_ms - 10.0).abs() < 0.01);
    assert!((stats.max_ms - 30.0).abs() < 0.01);
    assert_eq!(stats.count, 3);
}

/// Verify cold/warm comparison structure.
#[test]
fn test_cold_warm_comparison_structure() {
    let cold = vec![Measurement {
        start_type: "cold".to_string(),
        binary: "br".to_string(),
        command: "list".to_string(),
        run_index: 0,
        duration_ms: 50.0,
        exit_code: 0,
        success: true,
        stdout_bytes: 100,
    }];

    let warm = vec![Measurement {
        start_type: "warm".to_string(),
        binary: "br".to_string(),
        command: "list".to_string(),
        run_index: 0,
        duration_ms: 25.0,
        exit_code: 0,
        success: true,
        stdout_bytes: 100,
    }];

    let cold_stats = TimingStats::from_measurements(&cold);
    let warm_stats = TimingStats::from_measurements(&warm);

    let cold_warm_ratio = cold_stats.mean_ms / warm_stats.mean_ms;
    let startup_overhead_ms = cold_stats.mean_ms - warm_stats.mean_ms;

    // Cold should be ~2x warm
    assert!((cold_warm_ratio - 2.0).abs() < 0.01);
    // Overhead should be 25ms
    assert!((startup_overhead_ms - 25.0).abs() < 0.01);
}

/// Test summary calculation.
#[test]
fn test_summary_calculation() {
    let comparisons = vec![BinaryComparison {
        command: "list".to_string(),
        br_cold_warm: ColdWarmComparison {
            command: "list".to_string(),
            cold_stats: TimingStats {
                mean_ms: 50.0,
                median_ms: 50.0,
                min_ms: 48.0,
                max_ms: 52.0,
                stddev_ms: 2.0,
                count: 3,
            },
            warm_stats: TimingStats {
                mean_ms: 25.0,
                median_ms: 25.0,
                min_ms: 24.0,
                max_ms: 26.0,
                stddev_ms: 1.0,
                count: 5,
            },
            cold_warm_ratio: 2.0,
            startup_overhead_ms: 25.0,
            cold_measurements: vec![],
            warm_measurements: vec![],
        },
        bd_cold_warm: Some(ColdWarmComparison {
            command: "list".to_string(),
            cold_stats: TimingStats {
                mean_ms: 100.0,
                median_ms: 100.0,
                min_ms: 98.0,
                max_ms: 102.0,
                stddev_ms: 2.0,
                count: 3,
            },
            warm_stats: TimingStats {
                mean_ms: 50.0,
                median_ms: 50.0,
                min_ms: 48.0,
                max_ms: 52.0,
                stddev_ms: 2.0,
                count: 5,
            },
            cold_warm_ratio: 2.0,
            startup_overhead_ms: 50.0,
            cold_measurements: vec![],
            warm_measurements: vec![],
        }),
        cold_ratio: Some(0.5), // br is faster
        warm_ratio: Some(0.5), // br is faster
        overhead_ratio: Some(0.5),
    }];

    let summary = calculate_summary(&comparisons);

    assert!((summary.br_avg_cold_warm_ratio - 2.0).abs() < 0.01);
    assert!((summary.bd_avg_cold_warm_ratio.unwrap() - 2.0).abs() < 0.01);
    assert!((summary.br_avg_overhead_ms - 25.0).abs() < 0.01);
    assert!((summary.bd_avg_overhead_ms.unwrap() - 50.0).abs() < 0.01);
    assert_eq!(summary.br_faster_cold_count, 1);
    assert_eq!(summary.br_faster_warm_count, 1);
    assert_eq!(summary.total_commands, 1);
}
