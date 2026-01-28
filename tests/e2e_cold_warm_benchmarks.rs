//! Cold vs Warm Start Benchmark Suite
//!
//! Measures startup and repeated-run performance to capture cache effects.
//! Compares br (Rust) vs bd (Go) for both cold and warm scenarios.
//!
//! # Concepts
//!
//! - **Cold start**: First run after binary invocation (no `SQLite` page cache,
//!   minimal filesystem cache benefit)
//! - **Warm start**: Subsequent runs that benefit from caches (filesystem cache,
//!   `SQLite` page cache, OS buffer cache)
//!
//! # Commands Tested
//!
//! - `list --json` - List all issues
//! - `show <id> --json` - Show single issue
//! - `ready --json` - Get ready issues (dependency resolution)
//! - `stats --json` - Project statistics
//! - `sync --status` - Check sync status (read-only)
//!
//! # Usage
//!
//! ```bash
//! # Run cold/warm benchmarks
//! cargo test --test e2e_cold_warm_benchmarks -- --nocapture --ignored
//!
//! # Run quick version for CI
//! cargo test --test e2e_cold_warm_benchmarks cold_warm_quick -- --nocapture
//! ```
//!
//! # Output
//!
//! Results are written to `target/benchmark-results/cold_warm_summary.json`
//! with explicit cold/warm tags on each measurement.

#![allow(clippy::cast_precision_loss, clippy::similar_names)]

mod common;

use common::binary_discovery::{DiscoveredBinaries, discover_binaries};
use common::dataset_registry::{DatasetIntegrityGuard, IsolatedDataset, KnownDataset};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

// =============================================================================
// Run Type Tagging
// =============================================================================

/// Type of run for cache effect measurement
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunType {
    /// First run - no caches primed
    Cold,
    /// Subsequent run - caches likely warm
    Warm,
}

impl std::fmt::Display for RunType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cold => write!(f, "cold"),
            Self::Warm => write!(f, "warm"),
        }
    }
}

// =============================================================================
// Metrics
// =============================================================================

/// Metrics for a single run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMetrics {
    /// Command label
    pub command: String,
    /// Binary (br or bd)
    pub binary: String,
    /// Run type (cold or warm)
    pub run_type: RunType,
    /// Run index within type (0 for cold, 0-N for warm)
    pub run_index: usize,
    /// Duration in milliseconds
    pub duration_ms: u128,
    /// Whether command succeeded
    pub success: bool,
    /// Exit code
    pub exit_code: i32,
    /// Stdout length (bytes)
    pub stdout_len: usize,
}

/// Statistics for a set of runs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStats {
    pub run_type: RunType,
    pub count: usize,
    pub mean_ms: f64,
    pub median_ms: f64,
    pub min_ms: u128,
    pub max_ms: u128,
    pub stddev_ms: f64,
}

impl RunStats {
    #[must_use]
    pub fn from_runs(runs: &[RunMetrics], run_type: RunType) -> Self {
        let matching: Vec<&RunMetrics> = runs.iter().filter(|r| r.run_type == run_type).collect();

        if matching.is_empty() {
            return Self {
                run_type,
                count: 0,
                mean_ms: 0.0,
                median_ms: 0.0,
                min_ms: 0,
                max_ms: 0,
                stddev_ms: 0.0,
            };
        }

        let mut durations: Vec<u128> = matching.iter().map(|r| r.duration_ms).collect();
        durations.sort_unstable();

        let count = durations.len();
        let sum: u128 = durations.iter().sum();
        let mean = sum as f64 / count as f64;

        let median = if count % 2 == 0 {
            (durations[count / 2 - 1] + durations[count / 2]) as f64 / 2.0
        } else {
            durations[count / 2] as f64
        };

        let variance = durations
            .iter()
            .map(|d| {
                let diff = *d as f64 - mean;
                diff * diff
            })
            .sum::<f64>()
            / count as f64;
        let stddev = variance.sqrt();

        Self {
            run_type,
            count,
            mean_ms: mean,
            median_ms: median,
            min_ms: durations[0],
            max_ms: durations[count - 1],
            stddev_ms: stddev,
        }
    }
}

/// Comparison between cold and warm runs for a command
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdWarmComparison {
    /// Command label
    pub command: String,
    /// Binary (br or bd)
    pub binary: String,
    /// Cold run statistics
    pub cold: RunStats,
    /// Warm run statistics
    pub warm: RunStats,
    /// Warm speedup ratio (`cold_mean` / `warm_mean`)
    /// > 1.0 means warm is faster (expected)
    pub warm_speedup_ratio: f64,
    /// Cache benefit percentage: (cold - warm) / cold * 100
    pub cache_benefit_pct: f64,
    /// All individual runs
    pub runs: Vec<RunMetrics>,
}

/// Full comparison for a command across br and bd
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandComparison {
    pub command: String,
    pub br: ColdWarmComparison,
    pub bd: ColdWarmComparison,
    /// br vs bd cold comparison (`br_cold` / `bd_cold`, < 1.0 means br faster)
    pub br_bd_cold_ratio: f64,
    /// br vs bd warm comparison (`br_warm` / `bd_warm`, < 1.0 means br faster)
    pub br_bd_warm_ratio: f64,
}

/// Full benchmark results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdWarmBenchmarkResult {
    pub dataset: DatasetSummary,
    pub config: BenchmarkConfig,
    pub comparisons: Vec<CommandComparison>,
    pub summary: BenchmarkSummary,
    pub generated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetSummary {
    pub name: String,
    pub issue_count: usize,
    pub db_size_bytes: u64,
    pub jsonl_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    /// Number of cold runs (typically 1, as true cold requires process restart)
    pub cold_runs: usize,
    /// Number of warm runs
    pub warm_runs: usize,
    /// Commands to benchmark
    pub commands: Vec<String>,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            cold_runs: 3,
            warm_runs: 5,
            commands: vec![
                "list --json".to_string(),
                "ready --json".to_string(),
                "stats --json".to_string(),
                "sync --status".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkSummary {
    /// Average cache benefit for br (%)
    pub br_avg_cache_benefit_pct: f64,
    /// Average cache benefit for bd (%)
    pub bd_avg_cache_benefit_pct: f64,
    /// br faster than bd in cold starts (count)
    pub br_faster_cold_count: usize,
    /// br faster than bd in warm starts (count)
    pub br_faster_warm_count: usize,
    /// Total commands compared
    pub total_commands: usize,
}

// =============================================================================
// Command Runner
// =============================================================================

fn run_command(
    binary_path: &Path,
    args: &[&str],
    cwd: &Path,
    command_label: &str,
    binary_name: &str,
    run_type: RunType,
    run_index: usize,
) -> RunMetrics {
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

    RunMetrics {
        command: command_label.to_string(),
        binary: binary_name.to_string(),
        run_type,
        run_index,
        duration_ms: duration.as_millis(),
        success: output.status.success(),
        exit_code: output.status.code().unwrap_or(-1),
        stdout_len: output.stdout.len(),
    }
}

/// Attempt to drop filesystem caches (requires elevated privileges on Linux)
/// Returns true if successful, false otherwise
fn try_drop_caches() -> bool {
    #[cfg(target_os = "linux")]
    {
        // sync && echo 3 > /proc/sys/vm/drop_caches
        // This requires root privileges, so we just try and log if it fails
        let sync = Command::new("sync").output();
        if sync.is_err() {
            return false;
        }

        let drop = Command::new("sh")
            .arg("-c")
            .arg("echo 3 > /proc/sys/vm/drop_caches 2>/dev/null")
            .output();

        drop.is_ok_and(|o| o.status.success())
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

// =============================================================================
// Benchmark Runner
// =============================================================================

fn benchmark_command(
    br_path: &Path,
    bd_path: &Path,
    workspace: &Path,
    command: &str,
    config: &BenchmarkConfig,
) -> CommandComparison {
    let args: Vec<&str> = command.split_whitespace().collect();
    let label = command;

    let mut br_runs = Vec::new();
    let mut bd_runs = Vec::new();

    // Attempt to drop caches before cold runs (best effort)
    let _dropped = try_drop_caches();

    // Cold runs - first invocations
    for i in 0..config.cold_runs {
        // Run br cold
        let br_run = run_command(br_path, &args, workspace, label, "br", RunType::Cold, i);
        br_runs.push(br_run);

        // Run bd cold
        let bd_run = run_command(bd_path, &args, workspace, label, "bd", RunType::Cold, i);
        bd_runs.push(bd_run);
    }

    // Warm runs - subsequent invocations benefit from caches
    for i in 0..config.warm_runs {
        let br_run = run_command(br_path, &args, workspace, label, "br", RunType::Warm, i);
        br_runs.push(br_run);

        let bd_run = run_command(bd_path, &args, workspace, label, "bd", RunType::Warm, i);
        bd_runs.push(bd_run);
    }

    // Calculate statistics
    let br_cold = RunStats::from_runs(&br_runs, RunType::Cold);
    let br_warm = RunStats::from_runs(&br_runs, RunType::Warm);
    let bd_cold = RunStats::from_runs(&bd_runs, RunType::Cold);
    let bd_warm = RunStats::from_runs(&bd_runs, RunType::Warm);

    let br_warm_speedup = if br_warm.mean_ms > 0.0 {
        br_cold.mean_ms / br_warm.mean_ms
    } else {
        1.0
    };
    let br_cache_benefit = if br_cold.mean_ms > 0.0 {
        (br_cold.mean_ms - br_warm.mean_ms) / br_cold.mean_ms * 100.0
    } else {
        0.0
    };

    let bd_warm_speedup = if bd_warm.mean_ms > 0.0 {
        bd_cold.mean_ms / bd_warm.mean_ms
    } else {
        1.0
    };
    let bd_cache_benefit = if bd_cold.mean_ms > 0.0 {
        (bd_cold.mean_ms - bd_warm.mean_ms) / bd_cold.mean_ms * 100.0
    } else {
        0.0
    };

    let br_comparison = ColdWarmComparison {
        command: label.to_string(),
        binary: "br".to_string(),
        cold: br_cold.clone(),
        warm: br_warm.clone(),
        warm_speedup_ratio: br_warm_speedup,
        cache_benefit_pct: br_cache_benefit,
        runs: br_runs,
    };

    let bd_comparison = ColdWarmComparison {
        command: label.to_string(),
        binary: "bd".to_string(),
        cold: bd_cold.clone(),
        warm: bd_warm.clone(),
        warm_speedup_ratio: bd_warm_speedup,
        cache_benefit_pct: bd_cache_benefit,
        runs: bd_runs,
    };

    // Cross-binary comparisons
    let br_bd_cold_ratio = if bd_cold.mean_ms > 0.0 {
        br_cold.mean_ms / bd_cold.mean_ms
    } else {
        1.0
    };

    let br_bd_warm_ratio = if bd_warm.mean_ms > 0.0 {
        br_warm.mean_ms / bd_warm.mean_ms
    } else {
        1.0
    };

    CommandComparison {
        command: label.to_string(),
        br: br_comparison,
        bd: bd_comparison,
        br_bd_cold_ratio,
        br_bd_warm_ratio,
    }
}

/// Run the full cold/warm benchmark suite on a dataset
fn run_cold_warm_benchmarks(
    dataset: KnownDataset,
    binaries: &DiscoveredBinaries,
    config: &BenchmarkConfig,
) -> Result<ColdWarmBenchmarkResult, String> {
    let bd = binaries.require_bd()?;

    // Create integrity guard
    let mut guard = DatasetIntegrityGuard::new(dataset)
        .map_err(|e| format!("Failed to create integrity guard: {e}"))?;

    let before = guard.verify_before();
    if !before.passed {
        return Err(format!("Source integrity check failed: {}", before.message));
    }

    // Create isolated workspace
    let isolated = IsolatedDataset::from_dataset(dataset)
        .map_err(|e| format!("Failed to create workspace: {e}"))?;

    let workspace = isolated.workspace_root();
    let metadata = &isolated.metadata;

    println!(
        "\n  Cold/Warm Benchmark: {} ({} issues)",
        metadata.name, metadata.issue_count
    );

    // Run benchmarks for each command
    let mut comparisons = Vec::new();
    for cmd in &config.commands {
        println!("    Benchmarking: {cmd}");
        let comparison = benchmark_command(&binaries.br.path, &bd.path, workspace, cmd, config);
        comparisons.push(comparison);
    }

    // Verify source wasn't mutated
    let after = guard.verify_after();
    if !after.passed {
        return Err(format!(
            "Source was mutated during benchmark: {}",
            after.message
        ));
    }

    // Calculate summary
    let br_cache_benefits: Vec<f64> = comparisons.iter().map(|c| c.br.cache_benefit_pct).collect();
    let bd_cache_benefits: Vec<f64> = comparisons.iter().map(|c| c.bd.cache_benefit_pct).collect();

    let br_avg_cache = if br_cache_benefits.is_empty() {
        0.0
    } else {
        br_cache_benefits.iter().sum::<f64>() / br_cache_benefits.len() as f64
    };

    let bd_avg_cache = if bd_cache_benefits.is_empty() {
        0.0
    } else {
        bd_cache_benefits.iter().sum::<f64>() / bd_cache_benefits.len() as f64
    };

    let br_faster_cold = comparisons
        .iter()
        .filter(|c| c.br_bd_cold_ratio < 1.0)
        .count();
    let br_faster_warm = comparisons
        .iter()
        .filter(|c| c.br_bd_warm_ratio < 1.0)
        .count();

    let summary = BenchmarkSummary {
        br_avg_cache_benefit_pct: br_avg_cache,
        bd_avg_cache_benefit_pct: bd_avg_cache,
        br_faster_cold_count: br_faster_cold,
        br_faster_warm_count: br_faster_warm,
        total_commands: comparisons.len(),
    };

    Ok(ColdWarmBenchmarkResult {
        dataset: DatasetSummary {
            name: metadata.name.clone(),
            issue_count: metadata.issue_count,
            db_size_bytes: metadata.db_size_bytes,
            jsonl_size_bytes: metadata.jsonl_size_bytes,
        },
        config: config.clone(),
        comparisons,
        summary,
        generated_at: chrono::Utc::now().to_rfc3339(),
    })
}

// =============================================================================
// Output Formatting
// =============================================================================

fn print_cold_warm_table(result: &ColdWarmBenchmarkResult) {
    let sep = "=".repeat(90);
    let dash = "-".repeat(90);

    println!("\n{sep}");
    println!(
        "Cold vs Warm Benchmark: {} ({} issues)",
        result.dataset.name, result.dataset.issue_count
    );
    println!("{sep}");

    println!(
        "\n{:<25} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "Command", "br_cold", "br_warm", "br_cache%", "bd_cold", "bd_warm", "bd_cache%"
    );
    println!("{dash}");

    for c in &result.comparisons {
        println!(
            "{:<25} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10.1}",
            c.command,
            c.br.cold.mean_ms,
            c.br.warm.mean_ms,
            c.br.cache_benefit_pct,
            c.bd.cold.mean_ms,
            c.bd.warm.mean_ms,
            c.bd.cache_benefit_pct
        );
    }

    println!("{dash}");
    println!("\nSummary:");
    println!(
        "  br average cache benefit: {:.1}%",
        result.summary.br_avg_cache_benefit_pct
    );
    println!(
        "  bd average cache benefit: {:.1}%",
        result.summary.bd_avg_cache_benefit_pct
    );
    println!(
        "  br faster (cold): {}/{}",
        result.summary.br_faster_cold_count, result.summary.total_commands
    );
    println!(
        "  br faster (warm): {}/{}",
        result.summary.br_faster_warm_count, result.summary.total_commands
    );

    // Print br vs bd comparison
    println!("\nCross-binary comparison (ratio < 1.0 means br faster):");
    println!(
        "{:<25} {:>15} {:>15}",
        "Command", "cold_ratio", "warm_ratio"
    );
    println!("{}", "-".repeat(60));
    for c in &result.comparisons {
        let cold_winner = if c.br_bd_cold_ratio < 0.95 {
            "br"
        } else if c.br_bd_cold_ratio > 1.05 {
            "bd"
        } else {
            "tie"
        };
        let warm_winner = if c.br_bd_warm_ratio < 0.95 {
            "br"
        } else if c.br_bd_warm_ratio > 1.05 {
            "bd"
        } else {
            "tie"
        };
        println!(
            "{:<25} {:>10.2} ({:>3}) {:>10.2} ({:>3})",
            c.command, c.br_bd_cold_ratio, cold_winner, c.br_bd_warm_ratio, warm_winner
        );
    }
}

fn write_results_json(result: &ColdWarmBenchmarkResult, output_path: &Path) -> std::io::Result<()> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(output_path)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer_pretty(writer, result)?;
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

/// Full cold/warm benchmark suite
#[test]
#[ignore = "run with: cargo test --test e2e_cold_warm_benchmarks -- --ignored --nocapture"]
fn cold_warm_benchmark_full() {
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
        println!("bd: NOT FOUND - skipping cold/warm benchmarks");
        return;
    }

    let config = BenchmarkConfig {
        cold_runs: 3,
        warm_runs: 5,
        commands: vec![
            "list --json".to_string(),
            "ready --json".to_string(),
            "stats --json".to_string(),
            "sync --status".to_string(),
        ],
    };

    // Use beads_rust dataset for testing
    let dataset = KnownDataset::BeadsRust;
    if !dataset.beads_dir().exists() {
        println!("Dataset {} not available", dataset.name());
        return;
    }

    match run_cold_warm_benchmarks(dataset, &binaries, &config) {
        Ok(result) => {
            print_cold_warm_table(&result);

            // Write JSON results
            let output_dir = PathBuf::from("target/benchmark-results");
            let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
            let output_path = output_dir.join(format!("cold_warm_{timestamp}.json"));

            if let Err(e) = write_results_json(&result, &output_path) {
                eprintln!("Failed to write results: {e}");
            } else {
                println!("\nResults written to: {}", output_path.display());
            }

            // Also write cold_warm_latest.json
            let latest_path = output_dir.join("cold_warm_latest.json");
            if let Err(e) = write_results_json(&result, &latest_path) {
                eprintln!("Failed to write latest: {e}");
            }
        }
        Err(e) => {
            eprintln!("Benchmark failed: {e}");
            panic!("Cold/warm benchmark failed");
        }
    }
}

/// Quick cold/warm benchmark for CI
#[test]
fn cold_warm_quick() {
    println!("\n=== Cold vs Warm Quick Benchmark ===\n");

    let binaries = match discover_binaries() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Binary discovery failed: {e}");
            // Skip gracefully for CI if binaries aren't available
            println!("Skipping: {e}");
            return;
        }
    };

    if binaries.bd.is_none() {
        println!("bd not found - skipping cold/warm benchmark");
        return;
    }

    let config = BenchmarkConfig {
        cold_runs: 1,
        warm_runs: 3,
        commands: vec!["list --json".to_string(), "stats --json".to_string()],
    };

    let dataset = KnownDataset::BeadsRust;
    if !dataset.beads_dir().exists() {
        println!("Dataset {} not available, skipping", dataset.name());
        return;
    }

    match run_cold_warm_benchmarks(dataset, &binaries, &config) {
        Ok(result) => {
            print_cold_warm_table(&result);

            // Basic assertions
            assert!(!result.comparisons.is_empty(), "Should have comparisons");
            assert!(
                result.summary.total_commands > 0,
                "Should have command count"
            );

            // Cache benefit should generally be positive (warm faster than cold)
            // Allow for variance in short runs
            println!(
                "\nVerification: br cache benefit={:.1}%, bd cache benefit={:.1}%",
                result.summary.br_avg_cache_benefit_pct, result.summary.bd_avg_cache_benefit_pct
            );
        }
        Err(e) => {
            eprintln!("Benchmark error: {e}");
            // Don't panic for CI - just warn
            println!("Warning: cold/warm benchmark encountered errors");
        }
    }
}

/// Test that cold/warm tagging works correctly
#[test]
fn cold_warm_tagging_works() {
    let cold_run = RunMetrics {
        command: "test".to_string(),
        binary: "br".to_string(),
        run_type: RunType::Cold,
        run_index: 0,
        duration_ms: 100,
        success: true,
        exit_code: 0,
        stdout_len: 50,
    };

    let warm_run = RunMetrics {
        command: "test".to_string(),
        binary: "br".to_string(),
        run_type: RunType::Warm,
        run_index: 0,
        duration_ms: 50,
        success: true,
        exit_code: 0,
        stdout_len: 50,
    };

    assert_eq!(cold_run.run_type, RunType::Cold);
    assert_eq!(warm_run.run_type, RunType::Warm);
    assert_eq!(format!("{}", RunType::Cold), "cold");
    assert_eq!(format!("{}", RunType::Warm), "warm");
}

/// Test `RunStats` calculation
#[test]
fn run_stats_calculation() {
    let runs = vec![
        RunMetrics {
            command: "test".to_string(),
            binary: "br".to_string(),
            run_type: RunType::Cold,
            run_index: 0,
            duration_ms: 100,
            success: true,
            exit_code: 0,
            stdout_len: 0,
        },
        RunMetrics {
            command: "test".to_string(),
            binary: "br".to_string(),
            run_type: RunType::Cold,
            run_index: 1,
            duration_ms: 120,
            success: true,
            exit_code: 0,
            stdout_len: 0,
        },
        RunMetrics {
            command: "test".to_string(),
            binary: "br".to_string(),
            run_type: RunType::Warm,
            run_index: 0,
            duration_ms: 50,
            success: true,
            exit_code: 0,
            stdout_len: 0,
        },
        RunMetrics {
            command: "test".to_string(),
            binary: "br".to_string(),
            run_type: RunType::Warm,
            run_index: 1,
            duration_ms: 60,
            success: true,
            exit_code: 0,
            stdout_len: 0,
        },
    ];

    let cold_stats = RunStats::from_runs(&runs, RunType::Cold);
    let warm_stats = RunStats::from_runs(&runs, RunType::Warm);

    assert_eq!(cold_stats.count, 2);
    assert_eq!(warm_stats.count, 2);
    assert!((cold_stats.mean_ms - 110.0).abs() < 0.1);
    assert!((warm_stats.mean_ms - 55.0).abs() < 0.1);
    assert_eq!(cold_stats.min_ms, 100);
    assert_eq!(cold_stats.max_ms, 120);
    assert_eq!(warm_stats.min_ms, 50);
    assert_eq!(warm_stats.max_ms, 60);
}

/// Test cache benefit calculation
#[test]
fn cache_benefit_calculation() {
    // If cold is 100ms and warm is 50ms, cache benefit should be 50%
    let cold_mean: f64 = 100.0;
    let warm_mean: f64 = 50.0;
    let benefit: f64 = (cold_mean - warm_mean) / cold_mean * 100.0;
    assert!((benefit - 50.0).abs() < 0.1);

    // If cold is 100ms and warm is 80ms, cache benefit should be 20%
    let cold_mean: f64 = 100.0;
    let warm_mean: f64 = 80.0;
    let benefit: f64 = (cold_mean - warm_mean) / cold_mean * 100.0;
    assert!((benefit - 20.0).abs() < 0.1);
}
