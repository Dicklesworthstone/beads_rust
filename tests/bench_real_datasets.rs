//! Benchmark suite using real datasets from known repositories.
//!
//! This test module runs read-heavy and write-heavy workloads on real `.beads` datasets
//! and compares br (Rust) vs bd (Go) performance. All operations run on isolated copies
//! to ensure source datasets are never mutated.
//!
//! # Usage
//!
//! Run all benchmarks:
//! ```bash
//! cargo test --test bench_real_datasets -- --nocapture --ignored
//! ```
//!
//! Run with artifact logging:
//! ```bash
//! HARNESS_ARTIFACTS=1 cargo test --test bench_real_datasets -- --nocapture --ignored
//! ```
//!
//! # Workloads
//!
//! Read-heavy:
//! - `list --json` - List all issues
//! - `ready --json` - Get ready issues (dependency resolution)
//! - `stats --json` - Project statistics
//! - `search "keyword" --json` - Full-text search
//!
//! Write-heavy:
//! - `create` - Create new issues
//! - `update` - Update issue fields
//! - `close` - Close issues with reason

#![allow(clippy::cast_precision_loss, clippy::similar_names)]

mod common;

use common::binary_discovery::{DiscoveredBinaries, discover_binaries};
use common::dataset_registry::{
    DatasetIntegrityGuard, DatasetMetadata, IsolatedDataset, KnownDataset,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

// =============================================================================
// Metrics Collection
// =============================================================================

/// Metrics captured for a single command run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMetrics {
    /// Command label (e.g., "list", "ready")
    pub label: String,
    /// Binary used ("br" or "bd")
    pub binary: String,
    /// Wall-clock duration in milliseconds
    pub duration_ms: u128,
    /// Peak RSS in bytes (if available)
    pub peak_rss_bytes: Option<u64>,
    /// Exit code
    pub exit_code: i32,
    /// Whether command succeeded
    pub success: bool,
    /// Stdout length (proxy for output size)
    pub stdout_len: usize,
    /// Stderr length
    pub stderr_len: usize,
}

/// Comparison of br vs bd for a single operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comparison {
    pub label: String,
    pub br: RunMetrics,
    pub bd: RunMetrics,
    /// br/bd duration ratio (< 1.0 means br is faster)
    pub duration_ratio: f64,
    /// br/bd RSS ratio (< 1.0 means br uses less memory)
    pub rss_ratio: Option<f64>,
}

/// Full benchmark results for a dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetBenchmark {
    pub dataset: DatasetMetadataSummary,
    pub comparisons: Vec<Comparison>,
    pub summary: BenchmarkSummary,
}

/// Lightweight metadata summary for output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetMetadataSummary {
    pub name: String,
    pub issue_count: usize,
    pub dependency_count: usize,
    pub jsonl_size_bytes: u64,
    pub db_size_bytes: u64,
}

impl From<&DatasetMetadata> for DatasetMetadataSummary {
    fn from(m: &DatasetMetadata) -> Self {
        Self {
            name: m.name.clone(),
            issue_count: m.issue_count,
            dependency_count: m.dependency_count,
            jsonl_size_bytes: m.jsonl_size_bytes,
            db_size_bytes: m.db_size_bytes,
        }
    }
}

/// Summary statistics for a benchmark run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkSummary {
    /// Geometric mean of duration ratios (br/bd)
    pub geomean_duration_ratio: f64,
    /// Geometric mean of RSS ratios (br/bd)
    pub geomean_rss_ratio: Option<f64>,
    /// Number of operations where br was faster
    pub br_faster_count: usize,
    /// Number of operations where bd was faster
    pub bd_faster_count: usize,
    /// Total br time (ms)
    pub total_br_ms: u128,
    /// Total bd time (ms)
    pub total_bd_ms: u128,
}

// =============================================================================
// Command Runner with Metrics
// =============================================================================

/// Run a command and capture metrics including RSS.
fn run_with_metrics(
    binary_path: &Path,
    args: &[&str],
    cwd: &Path,
    label: &str,
    binary_name: &str,
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

    // Try to get peak RSS from /proc/self/status on Linux
    // For more accurate measurements, we'd need to use getrusage or external tools
    let peak_rss_bytes = get_peak_rss_bytes();

    RunMetrics {
        label: label.to_string(),
        binary: binary_name.to_string(),
        duration_ms: duration.as_millis(),
        peak_rss_bytes,
        exit_code: output.status.code().unwrap_or(-1),
        success: output.status.success(),
        stdout_len: output.stdout.len(),
        stderr_len: output.stderr.len(),
    }
}

/// Try to get peak RSS from the OS.
/// On Linux, reads /proc/self/status. Returns None on other platforms.
fn get_peak_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if line.starts_with("VmHWM:") {
                    // Format: "VmHWM:    123456 kB"
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        if let Ok(kb) = parts[1].parse::<u64>() {
                            return Some(kb * 1024);
                        }
                    }
                }
            }
        }
        None
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

// =============================================================================
// Benchmark Workloads
// =============================================================================

/// Run read-heavy workloads and return comparisons.
fn run_read_workloads(br_path: &Path, bd_path: &Path, workspace: &Path) -> Vec<Comparison> {
    let mut comparisons = Vec::new();

    // List all issues
    let br = run_with_metrics(br_path, &["list", "--json"], workspace, "list", "br");
    let bd = run_with_metrics(bd_path, &["list", "--json"], workspace, "list", "bd");
    comparisons.push(make_comparison("list", br, bd));

    // List with status filter
    let br = run_with_metrics(
        br_path,
        &["list", "--status=open", "--json"],
        workspace,
        "list_open",
        "br",
    );
    let bd = run_with_metrics(
        bd_path,
        &["list", "--status=open", "--json"],
        workspace,
        "list_open",
        "bd",
    );
    comparisons.push(make_comparison("list_open", br, bd));

    // Ready issues (dependency resolution)
    let br = run_with_metrics(br_path, &["ready", "--json"], workspace, "ready", "br");
    let bd = run_with_metrics(bd_path, &["ready", "--json"], workspace, "ready", "bd");
    comparisons.push(make_comparison("ready", br, bd));

    // Stats
    let br = run_with_metrics(br_path, &["stats", "--json"], workspace, "stats", "br");
    let bd = run_with_metrics(bd_path, &["stats", "--json"], workspace, "stats", "bd");
    comparisons.push(make_comparison("stats", br, bd));

    // Search (common term likely to exist)
    let br = run_with_metrics(
        br_path,
        &["search", "test", "--json"],
        workspace,
        "search",
        "br",
    );
    let bd = run_with_metrics(
        bd_path,
        &["search", "test", "--json"],
        workspace,
        "search",
        "bd",
    );
    comparisons.push(make_comparison("search", br, bd));

    // Show a specific issue (first one if exists)
    // We use `list --json` output to find an ID, but for simplicity just try a common prefix
    let br = run_with_metrics(
        br_path,
        &["list", "--limit=1", "--json"],
        workspace,
        "list_one",
        "br",
    );
    let bd = run_with_metrics(
        bd_path,
        &["list", "--limit=1", "--json"],
        workspace,
        "list_one",
        "bd",
    );
    comparisons.push(make_comparison("list_one", br, bd));

    comparisons
}

/// Run write-heavy workloads and return comparisons.
/// Note: These modify the workspace, so they should be run on an isolated copy.
fn run_write_workloads(
    br_path: &Path,
    bd_path: &Path,
    br_workspace: &Path,
    bd_workspace: &Path,
) -> Vec<Comparison> {
    let mut comparisons = Vec::new();

    // Create 10 issues
    for i in 0..10 {
        let title = format!("Benchmark issue {i}");
        let br = run_with_metrics(
            br_path,
            &["create", "--title", &title, "--type=task", "--priority=2"],
            br_workspace,
            &format!("create_{i}"),
            "br",
        );
        let bd = run_with_metrics(
            bd_path,
            &["create", "--title", &title, "--type=task", "--priority=2"],
            bd_workspace,
            &format!("create_{i}"),
            "bd",
        );
        comparisons.push(make_comparison(&format!("create_{i}"), br, bd));
    }

    // Aggregate create metrics
    let br_create_total: u128 = comparisons
        .iter()
        .filter(|c| c.label.starts_with("create_"))
        .map(|c| c.br.duration_ms)
        .sum();
    let bd_create_total: u128 = comparisons
        .iter()
        .filter(|c| c.label.starts_with("create_"))
        .map(|c| c.bd.duration_ms)
        .sum();

    // Add aggregate comparison
    comparisons.push(Comparison {
        label: "create_10_total".to_string(),
        br: RunMetrics {
            label: "create_10_total".to_string(),
            binary: "br".to_string(),
            duration_ms: br_create_total,
            peak_rss_bytes: None,
            exit_code: 0,
            success: true,
            stdout_len: 0,
            stderr_len: 0,
        },
        bd: RunMetrics {
            label: "create_10_total".to_string(),
            binary: "bd".to_string(),
            duration_ms: bd_create_total,
            peak_rss_bytes: None,
            exit_code: 0,
            success: true,
            stdout_len: 0,
            stderr_len: 0,
        },
        duration_ratio: if bd_create_total > 0 {
            br_create_total as f64 / bd_create_total as f64
        } else {
            1.0
        },
        rss_ratio: None,
    });

    // Note: update and close would need issue IDs from the creates above
    // For now, we focus on create operations as the primary write workload

    comparisons
}

/// Create a comparison from br and bd metrics.
fn make_comparison(label: &str, br: RunMetrics, bd: RunMetrics) -> Comparison {
    let duration_ratio = if bd.duration_ms > 0 {
        br.duration_ms as f64 / bd.duration_ms as f64
    } else if br.duration_ms > 0 {
        f64::INFINITY
    } else {
        1.0
    };

    let rss_ratio = match (br.peak_rss_bytes, bd.peak_rss_bytes) {
        (Some(br_rss), Some(bd_rss)) if bd_rss > 0 => Some(br_rss as f64 / bd_rss as f64),
        _ => None,
    };

    Comparison {
        label: label.to_string(),
        br,
        bd,
        duration_ratio,
        rss_ratio,
    }
}

/// Calculate summary statistics from comparisons.
fn calculate_summary(comparisons: &[Comparison]) -> BenchmarkSummary {
    let duration_ratios: Vec<f64> = comparisons
        .iter()
        .filter(|c| c.duration_ratio.is_finite() && c.duration_ratio > 0.0)
        .map(|c| c.duration_ratio)
        .collect();

    let rss_ratios: Vec<f64> = comparisons
        .iter()
        .filter_map(|c| c.rss_ratio)
        .filter(|r| r.is_finite() && *r > 0.0)
        .collect();

    #[allow(clippy::cast_precision_loss)]
    let geomean_duration_ratio = if duration_ratios.is_empty() {
        1.0
    } else {
        let log_sum: f64 = duration_ratios.iter().map(|r| r.ln()).sum();
        (log_sum / duration_ratios.len() as f64).exp()
    };

    #[allow(clippy::cast_precision_loss)]
    let geomean_rss_ratio = if rss_ratios.is_empty() {
        None
    } else {
        let log_sum: f64 = rss_ratios.iter().map(|r| r.ln()).sum();
        Some((log_sum / rss_ratios.len() as f64).exp())
    };

    let br_faster_count = comparisons
        .iter()
        .filter(|c| c.duration_ratio < 1.0)
        .count();
    let bd_faster_count = comparisons
        .iter()
        .filter(|c| c.duration_ratio > 1.0)
        .count();

    let total_br_ms: u128 = comparisons.iter().map(|c| c.br.duration_ms).sum();
    let total_bd_ms: u128 = comparisons.iter().map(|c| c.bd.duration_ms).sum();

    BenchmarkSummary {
        geomean_duration_ratio,
        geomean_rss_ratio,
        br_faster_count,
        bd_faster_count,
        total_br_ms,
        total_bd_ms,
    }
}

// =============================================================================
// Output Formatting
// =============================================================================

/// Print a comparison table to stdout.
fn print_comparison_table(benchmark: &DatasetBenchmark) {
    let sep = "=".repeat(80);
    let dash = "-".repeat(80);
    let name = &benchmark.dataset.name;
    let issues = benchmark.dataset.issue_count;
    let deps = benchmark.dataset.dependency_count;
    let jsonl_kb = benchmark.dataset.jsonl_size_bytes as f64 / 1024.0;
    let db_kb = benchmark.dataset.db_size_bytes as f64 / 1024.0;

    println!("\n{sep}");
    println!(
        "Dataset: {name} ({issues} issues, {deps} deps, {jsonl_kb:.1} KB JSONL, {db_kb:.1} KB DB)"
    );
    println!("{sep}");
    println!(
        "{:<20} {:>12} {:>12} {:>12} {:>10}",
        "Operation", "br (ms)", "bd (ms)", "Ratio", "Winner"
    );
    println!("{dash}");

    for c in &benchmark.comparisons {
        let winner = if c.duration_ratio < 0.95 {
            "br"
        } else if c.duration_ratio > 1.05 {
            "bd"
        } else {
            "tie"
        };

        let label = &c.label;
        let br_ms = c.br.duration_ms;
        let bd_ms = c.bd.duration_ms;
        let ratio = c.duration_ratio;
        println!("{label:<20} {br_ms:>12} {bd_ms:>12} {ratio:>12.2} {winner:>10}");
    }

    let faster = benchmark.summary.br_faster_count;
    let total_ops = benchmark.comparisons.len();
    let geomean = benchmark.summary.geomean_duration_ratio;
    let br_time = benchmark.summary.total_br_ms;
    let bd_time = benchmark.summary.total_bd_ms;

    println!("{dash}");
    println!("Summary: br faster in {faster}/{total_ops} ops, geomean ratio: {geomean:.2}x");
    println!("Total time: br={br_time}ms, bd={bd_time}ms");
    println!();
}

/// Write benchmark results to JSON file.
fn write_results_json(benchmarks: &[DatasetBenchmark], output_path: &Path) -> std::io::Result<()> {
    let file = File::create(output_path)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer_pretty(writer, benchmarks)?;
    Ok(())
}

// =============================================================================
// Main Benchmark Runner
// =============================================================================

/// Run benchmarks on a single dataset.
fn benchmark_dataset(
    dataset: KnownDataset,
    binaries: &DiscoveredBinaries,
) -> Result<DatasetBenchmark, String> {
    let bd = binaries.require_bd()?;

    // Create integrity guard to ensure source isn't mutated
    let mut guard = DatasetIntegrityGuard::new(dataset)
        .map_err(|e| format!("Failed to create integrity guard: {e}"))?;

    let before = guard.verify_before();
    if !before.passed {
        return Err(format!("Source integrity check failed: {}", before.message));
    }

    // Create isolated copies for br and bd
    let br_isolated = IsolatedDataset::from_dataset(dataset)
        .map_err(|e| format!("Failed to create br workspace: {e}"))?;
    let bd_isolated = IsolatedDataset::from_dataset(dataset)
        .map_err(|e| format!("Failed to create bd workspace: {e}"))?;

    let metadata_summary = DatasetMetadataSummary::from(&br_isolated.metadata);

    let ds_name = dataset.name();
    let issue_count = br_isolated.metadata.issue_count;
    println!("\nBenchmarking {ds_name} ({issue_count} issues)...");

    // Run read workloads (same workspace is fine since reads don't modify)
    let mut comparisons =
        run_read_workloads(&binaries.br.path, &bd.path, br_isolated.workspace_root());

    // Run write workloads (separate workspaces since writes modify state)
    let write_comparisons = run_write_workloads(
        &binaries.br.path,
        &bd.path,
        br_isolated.workspace_root(),
        bd_isolated.workspace_root(),
    );
    comparisons.extend(write_comparisons);

    // Verify source wasn't mutated
    let after = guard.verify_after();
    if !after.passed {
        return Err(format!(
            "Source dataset was mutated during benchmark: {}",
            after.message
        ));
    }

    let summary = calculate_summary(&comparisons);

    Ok(DatasetBenchmark {
        dataset: metadata_summary,
        comparisons,
        summary,
    })
}

// =============================================================================
// Tests
// =============================================================================

/// Run benchmarks on all available datasets.
#[test]
#[ignore = "run with: cargo test --test bench_real_datasets -- --ignored --nocapture"]
fn benchmark_all_datasets() {
    println!("\n=== Real Dataset Benchmark Suite ===\n");

    // Discover binaries
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
        println!("bd: NOT FOUND - skipping br/bd comparisons");
        println!("Install bd from: https://github.com/steveyegge/beads");
        return;
    }

    let mut results: Vec<DatasetBenchmark> = Vec::new();

    for dataset in KnownDataset::all() {
        // Check if dataset exists
        if !dataset.beads_dir().exists() {
            let name = dataset.name();
            println!("\nSkipping {name} (not available)");
            continue;
        }

        match benchmark_dataset(*dataset, &binaries) {
            Ok(benchmark) => {
                print_comparison_table(&benchmark);
                results.push(benchmark);
            }
            Err(e) => {
                let name = dataset.name();
                eprintln!("\nFailed to benchmark {name}: {e}");
            }
        }
    }

    // Write JSON results
    if !results.is_empty() {
        let output_dir = PathBuf::from("target/benchmark-results");
        fs::create_dir_all(&output_dir).expect("create output dir");

        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let output_path = output_dir.join(format!("real_datasets_{timestamp}.json"));

        if let Err(e) = write_results_json(&results, &output_path) {
            eprintln!("Failed to write results: {e}");
        } else {
            println!("\nResults written to: {}", output_path.display());
        }

        // Also write latest.json for easy access
        let latest_path = output_dir.join("real_datasets_latest.json");
        if let Err(e) = write_results_json(&results, &latest_path) {
            eprintln!("Failed to write latest results: {e}");
        }
    }

    // Print overall summary
    if !results.is_empty() {
        println!("\n{}", "=".repeat(80));
        println!("OVERALL SUMMARY");
        println!("{}", "=".repeat(80));

        let all_ratios: Vec<f64> = results
            .iter()
            .flat_map(|b| b.comparisons.iter().map(|c| c.duration_ratio))
            .filter(|r| r.is_finite() && *r > 0.0)
            .collect();

        if !all_ratios.is_empty() {
            let log_sum: f64 = all_ratios.iter().map(|r| r.ln()).sum();
            #[allow(clippy::cast_precision_loss)]
            let overall_geomean = (log_sum / all_ratios.len() as f64).exp();

            let total_br: u128 = results.iter().map(|b| b.summary.total_br_ms).sum();
            let total_bd: u128 = results.iter().map(|b| b.summary.total_bd_ms).sum();

            let dataset_count = results.len();
            let op_count = all_ratios.len();
            println!("Datasets benchmarked: {dataset_count}");
            println!("Total operations: {op_count}");
            println!("Overall geomean ratio: {overall_geomean:.2}x");
            println!("Total br time: {total_br}ms");
            println!("Total bd time: {total_bd}ms");

            if overall_geomean < 1.0 {
                let pct = (1.0 - overall_geomean) * 100.0;
                println!("\nbr is {pct:.1}% faster than bd on average");
            } else {
                let pct = (overall_geomean - 1.0) * 100.0;
                println!("\nbd is {pct:.1}% faster than br on average");
            }
        }
    }
}

/// Benchmark only `beads_rust` dataset (faster iteration).
#[test]
#[ignore = "manual benchmark run"]
fn benchmark_beads_rust_only() {
    let binaries = discover_binaries().expect("Binary discovery failed");

    if binaries.bd.is_none() {
        println!("bd not found, skipping benchmark");
        return;
    }

    let benchmark =
        benchmark_dataset(KnownDataset::BeadsRust, &binaries).expect("Benchmark failed");

    print_comparison_table(&benchmark);
}

/// Unit test for metrics calculation.
#[test]
fn test_calculate_summary() {
    let comparisons = vec![
        Comparison {
            label: "op1".to_string(),
            br: RunMetrics {
                label: "op1".to_string(),
                binary: "br".to_string(),
                duration_ms: 100,
                peak_rss_bytes: Some(1000),
                exit_code: 0,
                success: true,
                stdout_len: 10,
                stderr_len: 0,
            },
            bd: RunMetrics {
                label: "op1".to_string(),
                binary: "bd".to_string(),
                duration_ms: 200,
                peak_rss_bytes: Some(2000),
                exit_code: 0,
                success: true,
                stdout_len: 10,
                stderr_len: 0,
            },
            duration_ratio: 0.5,
            rss_ratio: Some(0.5),
        },
        Comparison {
            label: "op2".to_string(),
            br: RunMetrics {
                label: "op2".to_string(),
                binary: "br".to_string(),
                duration_ms: 200,
                peak_rss_bytes: Some(2000),
                exit_code: 0,
                success: true,
                stdout_len: 10,
                stderr_len: 0,
            },
            bd: RunMetrics {
                label: "op2".to_string(),
                binary: "bd".to_string(),
                duration_ms: 100,
                peak_rss_bytes: Some(1000),
                exit_code: 0,
                success: true,
                stdout_len: 10,
                stderr_len: 0,
            },
            duration_ratio: 2.0,
            rss_ratio: Some(2.0),
        },
    ];

    let summary = calculate_summary(&comparisons);

    // Geomean of 0.5 and 2.0 is 1.0
    assert!((summary.geomean_duration_ratio - 1.0).abs() < 0.01);
    assert_eq!(summary.br_faster_count, 1);
    assert_eq!(summary.bd_faster_count, 1);
    assert_eq!(summary.total_br_ms, 300);
    assert_eq!(summary.total_bd_ms, 300);
}

/// Unit test for comparison creation.
#[test]
fn test_make_comparison() {
    let br = RunMetrics {
        label: "test".to_string(),
        binary: "br".to_string(),
        duration_ms: 100,
        peak_rss_bytes: Some(1024),
        exit_code: 0,
        success: true,
        stdout_len: 50,
        stderr_len: 0,
    };

    let bd = RunMetrics {
        label: "test".to_string(),
        binary: "bd".to_string(),
        duration_ms: 200,
        peak_rss_bytes: Some(2048),
        exit_code: 0,
        success: true,
        stdout_len: 50,
        stderr_len: 0,
    };

    let comparison = make_comparison("test", br, bd);

    assert_eq!(comparison.label, "test");
    assert!((comparison.duration_ratio - 0.5).abs() < 0.01);
    assert!((comparison.rss_ratio.unwrap() - 0.5).abs() < 0.01);
}
