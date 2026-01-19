//! Cold vs Warm Start Benchmark Suite
//!
//! Measures the difference between cold start (fresh process, first run) and warm start
//! (repeat runs) for both br (Rust) and bd (Go) implementations.
//!
//! # Usage
//!
//! Run all cold/warm benchmarks:
//! ```bash
//! cargo test --test bench_cold_warm_start -- --nocapture --ignored
//! ```
//!
//! Run with artifact logging:
//! ```bash
//! HARNESS_ARTIFACTS=1 cargo test --test bench_cold_warm_start -- --nocapture --ignored
//! ```
//!
//! # Metrics Captured
//!
//! - Cold start time (first execution after workspace setup)
//! - Warm start times (subsequent executions)
//! - Cold/warm ratio for each command
//! - Comparison between br and bd for cold and warm scenarios
//!
//! # Commands Tested
//!
//! - list --json
//! - show <id> --json
//! - ready --json
//! - stats --json
//! - sync --status

#![allow(clippy::cast_precision_loss, clippy::similar_names)]

mod common;

use common::binary_discovery::{DiscoveredBinaries, discover_binaries};
use common::dataset_registry::{IsolatedDataset, KnownDataset};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// =============================================================================
// Cold/Warm Metrics
// =============================================================================

/// Metrics for a cold vs warm comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdWarmMetrics {
    /// Command label
    pub command: String,
    /// Binary name (br or bd)
    pub binary: String,
    /// Cold start duration (first run, ms)
    pub cold_start_ms: u128,
    /// Warm start durations (subsequent runs, ms)
    pub warm_runs_ms: Vec<u128>,
    /// Average warm start duration (ms)
    pub warm_avg_ms: f64,
    /// Cold/warm ratio (> 1.0 means cold is slower)
    pub cold_warm_ratio: f64,
    /// Standard deviation of warm runs
    pub warm_std_dev_ms: f64,
    /// Whether all runs succeeded
    pub success: bool,
}

/// Comparison between br and bd for cold/warm behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdWarmComparison {
    pub command: String,
    pub br: ColdWarmMetrics,
    pub bd: ColdWarmMetrics,
    /// br cold / bd cold ratio (< 1.0 means br cold is faster)
    pub cold_ratio_br_bd: f64,
    /// br warm / bd warm ratio (< 1.0 means br warm is faster)
    pub warm_ratio_br_bd: f64,
}

/// Full benchmark results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdWarmBenchmark {
    /// Dataset info
    pub dataset_name: String,
    pub issue_count: usize,
    /// Comparisons for each command
    pub comparisons: Vec<ColdWarmComparison>,
    /// Summary statistics
    pub summary: ColdWarmSummary,
    /// Timestamp
    pub timestamp: String,
}

/// Summary of cold/warm benchmark results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdWarmSummary {
    /// Average cold/warm ratio across all commands for br
    pub br_avg_cold_warm_ratio: f64,
    /// Average cold/warm ratio across all commands for bd
    pub bd_avg_cold_warm_ratio: f64,
    /// Commands where br is faster cold
    pub br_faster_cold_count: usize,
    /// Commands where br is faster warm
    pub br_faster_warm_count: usize,
    /// Total commands tested
    pub total_commands: usize,
}

// =============================================================================
// Command Runner
// =============================================================================

/// Result of a single command run.
struct RunResult {
    duration: Duration,
    success: bool,
    #[allow(dead_code)]
    stdout: Vec<u8>,
}

/// Run a command and measure execution time.
fn run_command(binary_path: &Path, args: &[&str], cwd: &Path) -> RunResult {
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

    RunResult {
        duration,
        success: output.status.success(),
        stdout: output.stdout,
    }
}

/// Measure cold and warm start for a single command.
fn measure_cold_warm(
    binary_path: &Path,
    args: &[&str],
    cwd: &Path,
    binary_name: &str,
    command_label: &str,
    warm_runs: usize,
) -> ColdWarmMetrics {
    // Cold start: first run
    let cold_result = run_command(binary_path, args, cwd);
    let cold_start_ms = cold_result.duration.as_millis();

    // Warm starts: subsequent runs
    let mut warm_runs_ms = Vec::with_capacity(warm_runs);
    let mut all_success = cold_result.success;

    for _ in 0..warm_runs {
        let result = run_command(binary_path, args, cwd);
        warm_runs_ms.push(result.duration.as_millis());
        all_success = all_success && result.success;
    }

    // Calculate warm average
    let warm_avg_ms = if warm_runs_ms.is_empty() {
        0.0
    } else {
        warm_runs_ms.iter().sum::<u128>() as f64 / warm_runs_ms.len() as f64
    };

    // Calculate warm standard deviation
    let warm_std_dev_ms = if warm_runs_ms.len() < 2 {
        0.0
    } else {
        let variance = warm_runs_ms
            .iter()
            .map(|&x| (x as f64 - warm_avg_ms).powi(2))
            .sum::<f64>()
            / warm_runs_ms.len() as f64;
        variance.sqrt()
    };

    // Calculate cold/warm ratio
    let cold_warm_ratio = if warm_avg_ms > 0.0 {
        cold_start_ms as f64 / warm_avg_ms
    } else {
        1.0
    };

    ColdWarmMetrics {
        command: command_label.to_string(),
        binary: binary_name.to_string(),
        cold_start_ms,
        warm_runs_ms,
        warm_avg_ms,
        cold_warm_ratio,
        warm_std_dev_ms,
        success: all_success,
    }
}

// =============================================================================
// Workspace Setup
// =============================================================================

/// Create a fresh workspace with br initialized and populated.
fn create_br_workspace(br_path: &Path, issue_count: usize) -> std::io::Result<(TempDir, PathBuf)> {
    let temp_dir = TempDir::new()?;
    let root = temp_dir.path().to_path_buf();

    // Create minimal git scaffold
    fs::create_dir_all(root.join(".git"))?;
    fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n")?;

    // Initialize beads
    let init_output = Command::new(br_path)
        .args(["init"])
        .current_dir(&root)
        .output()?;

    if !init_output.status.success() {
        return Err(std::io::Error::other(format!(
            "br init failed: {}",
            String::from_utf8_lossy(&init_output.stderr)
        )));
    }

    // Create issues
    for i in 0..issue_count {
        let title = format!("Benchmark issue {i}");
        let priority = (i % 5).to_string();

        let _ = Command::new(br_path)
            .args(["create", "--title", &title, "--priority", &priority])
            .current_dir(&root)
            .output()?;
    }

    // Flush to JSONL for consistent state
    let _ = Command::new(br_path)
        .args(["sync", "--flush-only"])
        .current_dir(&root)
        .output()?;

    Ok((temp_dir, root))
}

/// Copy a br workspace for bd usage (same JSONL, fresh DB).
fn copy_workspace_for_bd(br_root: &Path, bd_path: &Path) -> std::io::Result<(TempDir, PathBuf)> {
    let temp_dir = TempDir::new()?;
    let root = temp_dir.path().to_path_buf();

    // Copy entire directory structure
    copy_dir_all(br_root, &root)?;

    // Remove br's database so bd creates its own
    let br_db = root.join(".beads").join("beads.db");
    if br_db.exists() {
        fs::remove_file(&br_db)?;
    }
    // Also remove WAL and SHM files if present
    let _ = fs::remove_file(root.join(".beads").join("beads.db-wal"));
    let _ = fs::remove_file(root.join(".beads").join("beads.db-shm"));

    // Import into bd's database
    let import_output = Command::new(bd_path)
        .args(["sync", "--import-only"])
        .current_dir(&root)
        .output()?;

    if !import_output.status.success() {
        return Err(std::io::Error::other(format!(
            "bd sync import failed: {}",
            String::from_utf8_lossy(&import_output.stderr)
        )));
    }

    Ok((temp_dir, root))
}

/// Recursively copy a directory.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if path.is_dir() {
            copy_dir_all(&path, &dst_path)?;
        } else {
            fs::copy(&path, &dst_path)?;
        }
    }
    Ok(())
}

/// Get a valid issue ID from the workspace.
fn get_first_issue_id(br_path: &Path, workspace: &Path) -> Option<String> {
    let output = Command::new(br_path)
        .args(["list", "--limit=1", "--json"])
        .current_dir(workspace)
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse JSON array and extract first id
    for line in stdout.lines() {
        if let Ok(issues) = serde_json::from_str::<Vec<serde_json::Value>>(line) {
            if let Some(first) = issues.first() {
                if let Some(id) = first.get("id").and_then(|v| v.as_str()) {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

// =============================================================================
// Benchmark Runner
// =============================================================================

/// Commands to benchmark.
const BENCHMARK_COMMANDS: &[(&str, &[&str])] = &[
    ("list", &["list", "--json"]),
    ("ready", &["ready", "--json"]),
    ("stats", &["stats", "--json"]),
    ("sync_status", &["sync", "--status"]),
];

/// Number of warm runs per command.
const WARM_RUNS: usize = 5;

/// Run cold/warm benchmarks for a single dataset.
fn benchmark_cold_warm(
    binaries: &DiscoveredBinaries,
    issue_count: usize,
) -> Result<ColdWarmBenchmark, String> {
    let bd = binaries.require_bd()?;

    eprintln!("Setting up workspace with {issue_count} issues...");

    // Create br workspace
    let (_br_temp, br_root) = create_br_workspace(&binaries.br.path, issue_count)
        .map_err(|e| format!("Failed to create br workspace: {e}"))?;

    // Copy for bd
    let (_bd_temp, bd_root) = copy_workspace_for_bd(&br_root, &bd.path)
        .map_err(|e| format!("Failed to create bd workspace: {e}"))?;

    // Get an issue ID for show command
    let issue_id = get_first_issue_id(&binaries.br.path, &br_root);

    let mut comparisons = Vec::new();

    // Run standard commands
    for (label, args) in BENCHMARK_COMMANDS {
        eprintln!("  Benchmarking {label}...");

        let br_metrics =
            measure_cold_warm(&binaries.br.path, args, &br_root, "br", label, WARM_RUNS);

        let bd_metrics = measure_cold_warm(&bd.path, args, &bd_root, "bd", label, WARM_RUNS);

        let cold_ratio_br_bd = if bd_metrics.cold_start_ms > 0 {
            br_metrics.cold_start_ms as f64 / bd_metrics.cold_start_ms as f64
        } else {
            1.0
        };

        let warm_ratio_br_bd = if bd_metrics.warm_avg_ms > 0.0 {
            br_metrics.warm_avg_ms / bd_metrics.warm_avg_ms
        } else {
            1.0
        };

        comparisons.push(ColdWarmComparison {
            command: label.to_string(),
            br: br_metrics,
            bd: bd_metrics,
            cold_ratio_br_bd,
            warm_ratio_br_bd,
        });
    }

    // Add show command if we have an issue ID
    if let Some(id) = issue_id {
        eprintln!("  Benchmarking show...");

        let show_args: Vec<&str> = vec!["show", &id, "--json"];
        let br_metrics = measure_cold_warm(
            &binaries.br.path,
            &show_args,
            &br_root,
            "br",
            "show",
            WARM_RUNS,
        );

        // Use same ID for bd (copied workspace)
        let bd_metrics = measure_cold_warm(&bd.path, &show_args, &bd_root, "bd", "show", WARM_RUNS);

        let cold_ratio_br_bd = if bd_metrics.cold_start_ms > 0 {
            br_metrics.cold_start_ms as f64 / bd_metrics.cold_start_ms as f64
        } else {
            1.0
        };

        let warm_ratio_br_bd = if bd_metrics.warm_avg_ms > 0.0 {
            br_metrics.warm_avg_ms / bd_metrics.warm_avg_ms
        } else {
            1.0
        };

        comparisons.push(ColdWarmComparison {
            command: "show".to_string(),
            br: br_metrics,
            bd: bd_metrics,
            cold_ratio_br_bd,
            warm_ratio_br_bd,
        });
    }

    // Calculate summary
    let br_cold_warm_ratios: Vec<f64> = comparisons.iter().map(|c| c.br.cold_warm_ratio).collect();
    let bd_cold_warm_ratios: Vec<f64> = comparisons.iter().map(|c| c.bd.cold_warm_ratio).collect();

    let br_avg_cold_warm_ratio = if br_cold_warm_ratios.is_empty() {
        1.0
    } else {
        br_cold_warm_ratios.iter().sum::<f64>() / br_cold_warm_ratios.len() as f64
    };

    let bd_avg_cold_warm_ratio = if bd_cold_warm_ratios.is_empty() {
        1.0
    } else {
        bd_cold_warm_ratios.iter().sum::<f64>() / bd_cold_warm_ratios.len() as f64
    };

    let br_faster_cold_count = comparisons
        .iter()
        .filter(|c| c.cold_ratio_br_bd < 1.0)
        .count();
    let br_faster_warm_count = comparisons
        .iter()
        .filter(|c| c.warm_ratio_br_bd < 1.0)
        .count();

    let summary = ColdWarmSummary {
        br_avg_cold_warm_ratio,
        bd_avg_cold_warm_ratio,
        br_faster_cold_count,
        br_faster_warm_count,
        total_commands: comparisons.len(),
    };

    let timestamp = chrono::Utc::now().to_rfc3339();

    Ok(ColdWarmBenchmark {
        dataset_name: format!("synthetic_{issue_count}"),
        issue_count,
        comparisons,
        summary,
        timestamp,
    })
}

// =============================================================================
// Output Formatting
// =============================================================================

/// Print benchmark results to stdout.
fn print_benchmark(benchmark: &ColdWarmBenchmark) {
    let sep = "=".repeat(100);
    let dash = "-".repeat(100);

    println!("\n{sep}");
    println!(
        "Cold vs Warm Start Benchmark: {} ({} issues)",
        benchmark.dataset_name, benchmark.issue_count
    );
    println!("{sep}");

    println!(
        "\n{:<15} {:>12} {:>12} {:>10} {:>12} {:>12} {:>10} {:>12} {:>12}",
        "Command",
        "br Cold(ms)",
        "br Warm(ms)",
        "br C/W",
        "bd Cold(ms)",
        "bd Warm(ms)",
        "bd C/W",
        "Cold br/bd",
        "Warm br/bd"
    );
    println!("{dash}");

    for c in &benchmark.comparisons {
        println!(
            "{:<15} {:>12} {:>12.1} {:>10.2}x {:>12} {:>12.1} {:>10.2}x {:>12.2}x {:>12.2}x",
            c.command,
            c.br.cold_start_ms,
            c.br.warm_avg_ms,
            c.br.cold_warm_ratio,
            c.bd.cold_start_ms,
            c.bd.warm_avg_ms,
            c.bd.cold_warm_ratio,
            c.cold_ratio_br_bd,
            c.warm_ratio_br_bd
        );
    }

    println!("{dash}");
    println!("\nSummary:");
    println!(
        "  br average cold/warm ratio: {:.2}x",
        benchmark.summary.br_avg_cold_warm_ratio
    );
    println!(
        "  bd average cold/warm ratio: {:.2}x",
        benchmark.summary.bd_avg_cold_warm_ratio
    );
    println!(
        "  br faster on cold start: {}/{} commands",
        benchmark.summary.br_faster_cold_count, benchmark.summary.total_commands
    );
    println!(
        "  br faster on warm start: {}/{} commands",
        benchmark.summary.br_faster_warm_count, benchmark.summary.total_commands
    );
    println!();
}

/// Write benchmark results to JSON file.
fn write_results_json(benchmarks: &[ColdWarmBenchmark], output_path: &Path) -> std::io::Result<()> {
    let file = File::create(output_path)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer_pretty(writer, benchmarks)?;
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

/// Cold vs warm benchmark with small dataset (50 issues).
#[test]
#[ignore = "manual benchmark: cargo test --test bench_cold_warm_start -- --ignored --nocapture"]
fn cold_warm_small() {
    println!("\n=== Cold vs Warm Start Benchmark: Small (50 issues) ===\n");

    let binaries = match discover_binaries() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Binary discovery failed: {e}");
            panic!("Cannot run benchmarks without br binary");
        }
    };

    if binaries.bd.is_none() {
        println!("bd not found, skipping benchmark");
        return;
    }

    match benchmark_cold_warm(&binaries, 50) {
        Ok(benchmark) => {
            print_benchmark(&benchmark);

            // Write results
            let output_dir = PathBuf::from("target/benchmark-results");
            fs::create_dir_all(&output_dir).expect("create output dir");
            let output_path = output_dir.join("cold_warm_small_latest.json");
            write_results_json(&[benchmark], &output_path).expect("write results");
            println!("Results written to: {}", output_path.display());
        }
        Err(e) => {
            eprintln!("Benchmark failed: {e}");
        }
    }
}

/// Cold vs warm benchmark with medium dataset (200 issues).
#[test]
#[ignore = "manual benchmark: cargo test --test bench_cold_warm_start -- --ignored --nocapture"]
fn cold_warm_medium() {
    println!("\n=== Cold vs Warm Start Benchmark: Medium (200 issues) ===\n");

    let binaries = match discover_binaries() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Binary discovery failed: {e}");
            panic!("Cannot run benchmarks without br binary");
        }
    };

    if binaries.bd.is_none() {
        println!("bd not found, skipping benchmark");
        return;
    }

    match benchmark_cold_warm(&binaries, 200) {
        Ok(benchmark) => {
            print_benchmark(&benchmark);

            let output_dir = PathBuf::from("target/benchmark-results");
            fs::create_dir_all(&output_dir).expect("create output dir");
            let output_path = output_dir.join("cold_warm_medium_latest.json");
            write_results_json(&[benchmark], &output_path).expect("write results");
            println!("Results written to: {}", output_path.display());
        }
        Err(e) => {
            eprintln!("Benchmark failed: {e}");
        }
    }
}

/// Cold vs warm benchmark with large dataset (500 issues).
#[test]
#[ignore = "manual benchmark: cargo test --test bench_cold_warm_start -- --ignored --nocapture"]
fn cold_warm_large() {
    println!("\n=== Cold vs Warm Start Benchmark: Large (500 issues) ===\n");

    let binaries = match discover_binaries() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Binary discovery failed: {e}");
            panic!("Cannot run benchmarks without br binary");
        }
    };

    if binaries.bd.is_none() {
        println!("bd not found, skipping benchmark");
        return;
    }

    match benchmark_cold_warm(&binaries, 500) {
        Ok(benchmark) => {
            print_benchmark(&benchmark);

            let output_dir = PathBuf::from("target/benchmark-results");
            fs::create_dir_all(&output_dir).expect("create output dir");
            let output_path = output_dir.join("cold_warm_large_latest.json");
            write_results_json(&[benchmark], &output_path).expect("write results");
            println!("Results written to: {}", output_path.display());
        }
        Err(e) => {
            eprintln!("Benchmark failed: {e}");
        }
    }
}

/// Run all cold/warm benchmarks.
#[test]
#[ignore = "manual benchmark: cargo test --test bench_cold_warm_start cold_warm_all -- --ignored --nocapture"]
fn cold_warm_all() {
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
        println!("bd: NOT FOUND - skipping benchmarks");
        return;
    }

    let mut all_benchmarks = Vec::new();

    for &issue_count in &[50, 200, 500] {
        println!("\n--- Testing with {issue_count} issues ---");

        match benchmark_cold_warm(&binaries, issue_count) {
            Ok(benchmark) => {
                print_benchmark(&benchmark);
                all_benchmarks.push(benchmark);
            }
            Err(e) => {
                eprintln!("Benchmark failed for {issue_count} issues: {e}");
            }
        }
    }

    // Write combined results
    if !all_benchmarks.is_empty() {
        let output_dir = PathBuf::from("target/benchmark-results");
        fs::create_dir_all(&output_dir).expect("create output dir");

        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let output_path = output_dir.join(format!("cold_warm_all_{timestamp}.json"));
        write_results_json(&all_benchmarks, &output_path).expect("write results");
        println!("\nAll results written to: {}", output_path.display());

        // Also write latest
        let latest_path = output_dir.join("cold_warm_all_latest.json");
        write_results_json(&all_benchmarks, &latest_path).expect("write latest");
    }

    // Print overall summary
    println!("\n{}", "=".repeat(100));
    println!("OVERALL SUMMARY");
    println!("{}", "=".repeat(100));

    for b in &all_benchmarks {
        println!("\n{}: {} issues", b.dataset_name, b.issue_count);
        println!(
            "  br cold/warm ratio: {:.2}x, bd cold/warm ratio: {:.2}x",
            b.summary.br_avg_cold_warm_ratio, b.summary.bd_avg_cold_warm_ratio
        );
        println!(
            "  br faster: cold {}/{}, warm {}/{}",
            b.summary.br_faster_cold_count,
            b.summary.total_commands,
            b.summary.br_faster_warm_count,
            b.summary.total_commands
        );
    }
}

/// Cold vs warm benchmark using real datasets.
#[test]
#[ignore = "manual benchmark: cargo test --test bench_cold_warm_start cold_warm_real_datasets -- --ignored --nocapture"]
#[allow(clippy::too_many_lines)]
fn cold_warm_real_datasets() {
    println!("\n=== Cold vs Warm Start Benchmark: Real Datasets ===\n");

    let binaries = match discover_binaries() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Binary discovery failed: {e}");
            panic!("Cannot run benchmarks without br binary");
        }
    };

    let bd = match binaries.require_bd() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("bd not found: {e}");
            return;
        }
    };

    println!(
        "br: {} ({})",
        binaries.br.path.display(),
        binaries.br.version
    );
    println!("bd: {} ({})", bd.path.display(), bd.version);

    let mut all_results = Vec::new();

    for dataset in KnownDataset::all() {
        if !dataset.beads_dir().exists() {
            println!("\nSkipping {} (not available)", dataset.name());
            continue;
        }

        println!("\n--- Dataset: {} ---", dataset.name());

        // Create isolated copies
        let br_isolated = match IsolatedDataset::from_dataset(*dataset) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to create br workspace: {e}");
                continue;
            }
        };

        let bd_isolated = match IsolatedDataset::from_dataset(*dataset) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to create bd workspace: {e}");
                continue;
            }
        };

        let issue_count = br_isolated.metadata.issue_count;
        let issue_id = get_first_issue_id(&binaries.br.path, br_isolated.workspace_root());

        let mut comparisons = Vec::new();

        for (label, args) in BENCHMARK_COMMANDS {
            eprintln!("  Benchmarking {label}...");

            let br_metrics = measure_cold_warm(
                &binaries.br.path,
                args,
                br_isolated.workspace_root(),
                "br",
                label,
                WARM_RUNS,
            );

            let bd_metrics = measure_cold_warm(
                &bd.path,
                args,
                bd_isolated.workspace_root(),
                "bd",
                label,
                WARM_RUNS,
            );

            let cold_ratio_br_bd = if bd_metrics.cold_start_ms > 0 {
                br_metrics.cold_start_ms as f64 / bd_metrics.cold_start_ms as f64
            } else {
                1.0
            };

            let warm_ratio_br_bd = if bd_metrics.warm_avg_ms > 0.0 {
                br_metrics.warm_avg_ms / bd_metrics.warm_avg_ms
            } else {
                1.0
            };

            comparisons.push(ColdWarmComparison {
                command: label.to_string(),
                br: br_metrics,
                bd: bd_metrics,
                cold_ratio_br_bd,
                warm_ratio_br_bd,
            });
        }

        // Add show command if we have an ID
        if let Some(id) = issue_id {
            eprintln!("  Benchmarking show...");
            let show_args: Vec<&str> = vec!["show", &id, "--json"];

            let br_metrics = measure_cold_warm(
                &binaries.br.path,
                &show_args,
                br_isolated.workspace_root(),
                "br",
                "show",
                WARM_RUNS,
            );

            let bd_metrics = measure_cold_warm(
                &bd.path,
                &show_args,
                bd_isolated.workspace_root(),
                "bd",
                "show",
                WARM_RUNS,
            );

            let cold_ratio_br_bd = if bd_metrics.cold_start_ms > 0 {
                br_metrics.cold_start_ms as f64 / bd_metrics.cold_start_ms as f64
            } else {
                1.0
            };

            let warm_ratio_br_bd = if bd_metrics.warm_avg_ms > 0.0 {
                br_metrics.warm_avg_ms / bd_metrics.warm_avg_ms
            } else {
                1.0
            };

            comparisons.push(ColdWarmComparison {
                command: "show".to_string(),
                br: br_metrics,
                bd: bd_metrics,
                cold_ratio_br_bd,
                warm_ratio_br_bd,
            });
        }

        // Calculate summary
        let br_cold_warm_ratios: Vec<f64> =
            comparisons.iter().map(|c| c.br.cold_warm_ratio).collect();
        let bd_cold_warm_ratios: Vec<f64> =
            comparisons.iter().map(|c| c.bd.cold_warm_ratio).collect();

        let br_avg_cold_warm_ratio = if br_cold_warm_ratios.is_empty() {
            1.0
        } else {
            br_cold_warm_ratios.iter().sum::<f64>() / br_cold_warm_ratios.len() as f64
        };

        let bd_avg_cold_warm_ratio = if bd_cold_warm_ratios.is_empty() {
            1.0
        } else {
            bd_cold_warm_ratios.iter().sum::<f64>() / bd_cold_warm_ratios.len() as f64
        };

        let summary = ColdWarmSummary {
            br_avg_cold_warm_ratio,
            bd_avg_cold_warm_ratio,
            br_faster_cold_count: comparisons
                .iter()
                .filter(|c| c.cold_ratio_br_bd < 1.0)
                .count(),
            br_faster_warm_count: comparisons
                .iter()
                .filter(|c| c.warm_ratio_br_bd < 1.0)
                .count(),
            total_commands: comparisons.len(),
        };

        let benchmark = ColdWarmBenchmark {
            dataset_name: dataset.name().to_string(),
            issue_count,
            comparisons,
            summary,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        print_benchmark(&benchmark);
        all_results.push(benchmark);
    }

    // Write combined results
    if !all_results.is_empty() {
        let output_dir = PathBuf::from("target/benchmark-results");
        fs::create_dir_all(&output_dir).expect("create output dir");

        let output_path = output_dir.join("cold_warm_real_datasets_latest.json");
        write_results_json(&all_results, &output_path).expect("write results");
        println!("\nResults written to: {}", output_path.display());
    }
}

/// Unit test for cold/warm ratio calculation.
#[test]
fn test_cold_warm_ratio() {
    // If cold is 100ms and warm average is 50ms, ratio should be 2.0
    let cold_start_ms: f64 = 100.0;
    let warm_avg_ms: f64 = 50.0;
    let ratio = cold_start_ms / warm_avg_ms;
    assert!((ratio - 2.0).abs() < 0.01);
}

/// Unit test for standard deviation calculation.
#[test]
fn test_std_dev_calculation() {
    let warm_runs_ms = [10u128, 12, 11, 13, 11];
    let warm_avg_ms = warm_runs_ms.iter().sum::<u128>() as f64 / warm_runs_ms.len() as f64;

    let variance = warm_runs_ms
        .iter()
        .map(|&x| (x as f64 - warm_avg_ms).powi(2))
        .sum::<f64>()
        / warm_runs_ms.len() as f64;
    let std_dev = variance.sqrt();

    // Expected: mean ~11.4, variance ~1.04, std_dev ~1.02
    assert!((warm_avg_ms - 11.4).abs() < 0.1);
    assert!(std_dev > 0.9 && std_dev < 1.2);
}
