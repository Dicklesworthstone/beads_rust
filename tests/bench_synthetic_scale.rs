//! Synthetic scale-up benchmark suite for stress testing with large datasets.
//!
//! This module generates synthetic datasets (100k+ issues) by expanding patterns
//! from real datasets, then exercises list/search/ready/sync operations at scale.
//!
//! # Usage
//!
//! These tests are opt-in only (long-running stress tests):
//! ```bash
//! BR_E2E_STRESS=1 cargo test --test bench_synthetic_scale -- --ignored --nocapture
//! ```
//!
//! # Metrics Captured
//!
//! - Wall-clock time for each operation
//! - Peak RSS (memory) on Linux
//! - Export/import file sizes
//! - Issue counts and dependency density
//!
//! # Scale Tiers
//!
//! - Small: 10,000 issues (quick sanity check)
//! - Medium: 50,000 issues
//! - Large: 100,000 issues
//! - XLarge: 250,000 issues (very long-running)

#![allow(
    clippy::cast_precision_loss,
    clippy::similar_names,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    clippy::too_many_lines,
    clippy::missing_const_for_fn
)]

mod common;

use common::binary_discovery::discover_binaries;
use common::dataset_registry::KnownDataset;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;
use tempfile::TempDir;

// =============================================================================
// Configuration
// =============================================================================

/// Check if stress tests are enabled.
fn stress_tests_enabled() -> bool {
    std::env::var("BR_E2E_STRESS").is_ok()
}

/// Scale tier for synthetic datasets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleTier {
    /// 10,000 issues - quick sanity check
    Small,
    /// 50,000 issues - medium stress
    Medium,
    /// 100,000 issues - standard stress test
    Large,
    /// 250,000 issues - extreme stress test
    XLarge,
}

impl ScaleTier {
    #[must_use]
    pub const fn issue_count(self) -> usize {
        match self {
            Self::Small => 10_000,
            Self::Medium => 50_000,
            Self::Large => 100_000,
            Self::XLarge => 250_000,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Small => "small_10k",
            Self::Medium => "medium_50k",
            Self::Large => "large_100k",
            Self::XLarge => "xlarge_250k",
        }
    }

    /// Target dependency density (deps per issue on average).
    #[must_use]
    pub const fn dependency_density(self) -> f64 {
        match self {
            Self::Small => 0.3,
            Self::Medium | Self::Large => 0.5,
            Self::XLarge => 0.7,
        }
    }
}

// =============================================================================
// Synthetic Dataset Generator
// =============================================================================

/// Configuration for synthetic dataset generation.
#[derive(Debug, Clone)]
pub struct SyntheticConfig {
    /// Target number of issues
    pub issue_count: usize,
    /// Average dependencies per issue (0.0 - 1.0)
    pub dependency_density: f64,
    /// Random seed for reproducibility
    pub seed: u64,
    /// Base dataset to expand (for realistic patterns)
    pub base_dataset: Option<KnownDataset>,
}

impl SyntheticConfig {
    #[must_use]
    pub fn from_tier(tier: ScaleTier) -> Self {
        Self {
            issue_count: tier.issue_count(),
            dependency_density: tier.dependency_density(),
            seed: 42, // Reproducible by default
            base_dataset: Some(KnownDataset::BeadsRust),
        }
    }

    #[must_use]
    pub const fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }
}

/// Metrics from synthetic dataset generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationMetrics {
    /// Actual issue count generated
    pub issue_count: usize,
    /// Actual dependency count generated
    pub dependency_count: usize,
    /// Generation duration
    pub generation_ms: u128,
    /// JSONL file size in bytes
    pub jsonl_size_bytes: u64,
    /// DB file size after rebuild
    pub db_size_bytes: u64,
}

/// A generated synthetic dataset in an isolated workspace.
pub struct SyntheticDataset {
    pub temp_dir: TempDir,
    pub root: PathBuf,
    pub beads_dir: PathBuf,
    pub config: SyntheticConfig,
    pub metrics: GenerationMetrics,
}

impl SyntheticDataset {
    /// Generate a synthetic dataset based on the config.
    ///
    /// # Errors
    ///
    /// Returns an error if the temporary workspace or any CLI command fails.
    pub fn generate(config: SyntheticConfig, br_path: &Path) -> std::io::Result<Self> {
        let start = Instant::now();
        let temp_dir = TempDir::new()?;
        let root = temp_dir.path().to_path_buf();
        let beads_dir = root.join(".beads");

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

        // Generate synthetic issues
        let mut rng = StdRng::seed_from_u64(config.seed);
        let mut dependency_count = 0;

        // Pre-compute issue IDs for dependency references (unused but kept for reference)
        let _issue_ids: Vec<String> = (0..config.issue_count)
            .map(|i| format!("synth-{i:08x}"))
            .collect();

        // Generate issues via br CLI (batch creates)
        let batch_size = 1000; // Create in batches to avoid command-line limits
        let mut created_count = 0;

        for batch_start in (0..config.issue_count).step_by(batch_size) {
            let batch_end = (batch_start + batch_size).min(config.issue_count);

            for i in batch_start..batch_end {
                let issue_type = match rng.random_range(0..10) {
                    0..=5 => "task",
                    6..=8 => "bug",
                    _ => "feature",
                };

                let priority = rng.random_range(0..=4);
                let title = generate_title(&mut rng, i);

                let create_output = Command::new(br_path)
                    .args([
                        "create",
                        "--title",
                        &title,
                        "--type",
                        issue_type,
                        "--priority",
                        &priority.to_string(),
                    ])
                    .current_dir(&root)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()?;

                if !create_output.status.success() {
                    eprintln!(
                        "Warning: create failed for issue {i}: {}",
                        String::from_utf8_lossy(&create_output.stderr)
                    );
                    continue;
                }

                created_count += 1;

                // Progress indicator
                if created_count % 5000 == 0 {
                    eprintln!(
                        "  Generated {created_count}/{} issues...",
                        config.issue_count
                    );
                }
            }
        }

        // Add dependencies (in a second pass to ensure all issues exist)
        if config.dependency_density > 0.0 {
            eprintln!("  Adding dependencies...");

            // Get list of actual issue IDs
            let list_output = Command::new(br_path)
                .args(["list", "--json"])
                .current_dir(&root)
                .output()?;

            if list_output.status.success() {
                let actual_ids: Vec<String> = String::from_utf8_lossy(&list_output.stdout)
                    .lines()
                    .filter_map(|line| {
                        serde_json::from_str::<serde_json::Value>(line)
                            .ok()
                            .and_then(|v| v["id"].as_str().map(String::from))
                    })
                    .collect();

                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let target_deps =
                    (actual_ids.len() as f64 * config.dependency_density).round() as usize;
                let mut added_deps = 0;

                for _ in 0..target_deps {
                    if actual_ids.len() < 2 {
                        break;
                    }

                    let from_idx = rng.random_range(1..actual_ids.len());
                    let to_idx = rng.random_range(0..from_idx); // Dependency on earlier issue

                    let dep_output = Command::new(br_path)
                        .args(["dep", "add", &actual_ids[from_idx], &actual_ids[to_idx]])
                        .current_dir(&root)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .output()?;

                    if dep_output.status.success() {
                        added_deps += 1;
                        dependency_count += 1;
                    }

                    if added_deps % 1000 == 0 && added_deps > 0 {
                        eprintln!("  Added {added_deps}/{target_deps} dependencies...");
                    }
                }
            }
        }

        let generation_ms = start.elapsed().as_millis();

        // Measure file sizes
        let jsonl_path = beads_dir.join("issues.jsonl");
        let db_path = beads_dir.join("beads.db");

        let jsonl_size_bytes = fs::metadata(&jsonl_path).map_or(0, |m| m.len());
        let db_size_bytes = fs::metadata(&db_path).map_or(0, |m| m.len());

        let metrics = GenerationMetrics {
            issue_count: created_count,
            dependency_count,
            generation_ms,
            jsonl_size_bytes,
            db_size_bytes,
        };

        Ok(Self {
            temp_dir,
            root,
            beads_dir,
            config,
            metrics,
        })
    }

    /// Get workspace root for command execution.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.root
    }
}

/// Generate a realistic-looking issue title.
fn generate_title(rng: &mut StdRng, index: usize) -> String {
    let prefixes = [
        "Add",
        "Fix",
        "Update",
        "Refactor",
        "Implement",
        "Remove",
        "Improve",
        "Optimize",
        "Document",
        "Test",
        "Review",
        "Debug",
        "Cleanup",
        "Migrate",
        "Configure",
    ];

    let subjects = [
        "authentication flow",
        "database connection",
        "API endpoint",
        "user interface",
        "error handling",
        "logging system",
        "configuration",
        "test coverage",
        "documentation",
        "performance",
        "security",
        "caching",
        "validation",
        "serialization",
        "routing",
    ];

    let prefix = prefixes[rng.random_range(0..prefixes.len())];
    let subject = subjects[rng.random_range(0..subjects.len())];

    format!("{prefix} {subject} (#{index})")
}

// =============================================================================
// Benchmark Metrics
// =============================================================================

/// Metrics for a single benchmark operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationMetrics {
    /// Operation name
    pub operation: String,
    /// Wall-clock duration in milliseconds
    pub duration_ms: u128,
    /// Peak RSS in bytes (Linux only)
    pub peak_rss_bytes: Option<u64>,
    /// Whether the operation succeeded
    pub success: bool,
    /// Output size in bytes
    pub output_size_bytes: usize,
    /// Error message if failed
    pub error: Option<String>,
}

/// Full benchmark results for a synthetic dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntheticBenchmark {
    /// Scale tier name
    pub tier: String,
    /// Dataset generation metrics
    pub generation: GenerationMetrics,
    /// Operation benchmarks
    pub operations: Vec<OperationMetrics>,
    /// Summary statistics
    pub summary: BenchmarkSummary,
    /// Timestamp
    pub timestamp: String,
}

/// Summary of benchmark results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkSummary {
    /// Total benchmark duration (including generation)
    pub total_duration_ms: u128,
    /// Average operation duration
    pub avg_operation_ms: u128,
    /// Slowest operation
    pub slowest_operation: String,
    /// Slowest operation duration
    pub slowest_duration_ms: u128,
    /// Operations per second (throughput)
    pub ops_per_second: f64,
    /// Issues per second (for list operations)
    pub issues_per_second: Option<f64>,
}

// =============================================================================
// Benchmark Runner
// =============================================================================

/// Run a command and capture metrics.
fn run_operation(
    br_path: &Path,
    args: &[&str],
    workspace: &Path,
    operation: &str,
) -> OperationMetrics {
    let start = Instant::now();

    let output = Command::new(br_path)
        .args(args)
        .current_dir(workspace)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    let duration = start.elapsed();

    match output {
        Ok(out) => {
            let success = out.status.success();
            let error = if success {
                None
            } else {
                Some(String::from_utf8_lossy(&out.stderr).to_string())
            };

            OperationMetrics {
                operation: operation.to_string(),
                duration_ms: duration.as_millis(),
                peak_rss_bytes: get_peak_rss_bytes(),
                success,
                output_size_bytes: out.stdout.len(),
                error,
            }
        }
        Err(e) => OperationMetrics {
            operation: operation.to_string(),
            duration_ms: duration.as_millis(),
            peak_rss_bytes: None,
            success: false,
            output_size_bytes: 0,
            error: Some(e.to_string()),
        },
    }
}

/// Get peak RSS from /proc/self/status on Linux.
fn get_peak_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if line.starts_with("VmHWM:") {
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

/// Run full benchmark suite on a synthetic dataset.
fn benchmark_synthetic(dataset: &SyntheticDataset, br_path: &Path) -> SyntheticBenchmark {
    let start = Instant::now();
    let mut operations = Vec::new();
    let workspace = dataset.workspace_root();

    // Read operations
    operations.push(run_operation(
        br_path,
        &["list", "--json"],
        workspace,
        "list",
    ));
    operations.push(run_operation(
        br_path,
        &["list", "--status=open", "--json"],
        workspace,
        "list_open",
    ));
    operations.push(run_operation(
        br_path,
        &["ready", "--json"],
        workspace,
        "ready",
    ));
    operations.push(run_operation(
        br_path,
        &["stats", "--json"],
        workspace,
        "stats",
    ));
    operations.push(run_operation(
        br_path,
        &["search", "test", "--json"],
        workspace,
        "search",
    ));
    operations.push(run_operation(
        br_path,
        &["blocked", "--json"],
        workspace,
        "blocked",
    ));

    // Export operation
    let export_path = dataset.root.join("export.jsonl");
    operations.push(run_operation(
        br_path,
        &["export", "--output", export_path.to_str().unwrap()],
        workspace,
        "export",
    ));

    // Measure export size (captured for future use in metrics)
    let _export_size = fs::metadata(&export_path).map_or(0, |m| m.len());

    // Calculate summary
    let total_duration_ms = start.elapsed().as_millis();
    let successful_ops: Vec<_> = operations.iter().filter(|o| o.success).collect();

    let avg_operation_ms = if successful_ops.is_empty() {
        0
    } else {
        successful_ops.iter().map(|o| o.duration_ms).sum::<u128>() / successful_ops.len() as u128
    };

    let (slowest_operation, slowest_duration_ms) =
        operations.iter().max_by_key(|o| o.duration_ms).map_or_else(
            || ("none".to_string(), 0),
            |o| (o.operation.clone(), o.duration_ms),
        );

    let ops_per_second = if total_duration_ms > 0 {
        (operations.len() as f64 * 1000.0) / total_duration_ms as f64
    } else {
        0.0
    };

    // Calculate issues/second for list operation
    let issues_per_second = operations
        .iter()
        .find(|o| o.operation == "list" && o.success)
        .map(|o| {
            if o.duration_ms > 0 {
                (dataset.metrics.issue_count as f64 * 1000.0) / o.duration_ms as f64
            } else {
                0.0
            }
        });

    let summary = BenchmarkSummary {
        total_duration_ms,
        avg_operation_ms,
        slowest_operation,
        slowest_duration_ms,
        ops_per_second,
        issues_per_second,
    };

    let timestamp = chrono::Utc::now().to_rfc3339();

    SyntheticBenchmark {
        tier: format!(
            "synthetic_{}",
            match dataset.config.issue_count {
                n if n <= 10_000 => "small",
                n if n <= 50_000 => "medium",
                n if n <= 100_000 => "large",
                _ => "xlarge",
            }
        ),
        generation: dataset.metrics.clone(),
        operations,
        summary,
        timestamp,
    }
}

/// Print benchmark results to stdout.
fn print_benchmark(benchmark: &SyntheticBenchmark) {
    let sep = "=".repeat(80);
    let dash = "-".repeat(80);

    println!("\n{sep}");
    println!("Synthetic Benchmark: {}", benchmark.tier);
    println!("{sep}");

    // Generation metrics
    let generation = &benchmark.generation;
    println!(
        "Dataset: {} issues, {} dependencies ({:.1} KB JSONL, {:.1} KB DB)",
        generation.issue_count,
        generation.dependency_count,
        generation.jsonl_size_bytes as f64 / 1024.0,
        generation.db_size_bytes as f64 / 1024.0
    );
    println!("Generation time: {}ms", generation.generation_ms);
    println!("{dash}");

    // Operations
    println!(
        "{:<20} {:>12} {:>12} {:>10}",
        "Operation", "Duration(ms)", "Output(KB)", "Status"
    );
    println!("{dash}");

    for op in &benchmark.operations {
        let status = if op.success { "OK" } else { "FAIL" };
        let output_kb = op.output_size_bytes as f64 / 1024.0;
        println!(
            "{:<20} {:>12} {:>12.1} {:>10}",
            op.operation, op.duration_ms, output_kb, status
        );
    }

    // Summary
    let sum = &benchmark.summary;
    println!("{dash}");
    println!("Total duration: {}ms", sum.total_duration_ms);
    println!("Avg operation: {}ms", sum.avg_operation_ms);
    println!(
        "Slowest: {} ({}ms)",
        sum.slowest_operation, sum.slowest_duration_ms
    );
    if let Some(ips) = sum.issues_per_second {
        println!("List throughput: {:.0} issues/second", ips);
    }
    println!();
}

/// Write benchmark results to JSON file.
fn write_benchmark_json(
    benchmarks: &[SyntheticBenchmark],
    output_path: &Path,
) -> std::io::Result<()> {
    let file = File::create(output_path)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer_pretty(writer, benchmarks)?;
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

/// Small scale synthetic benchmark (10k issues).
/// Env gate: BR_E2E_STRESS=1
#[test]
#[ignore = "stress test: BR_E2E_STRESS=1 cargo test --test bench_synthetic_scale -- --ignored"]
fn stress_synthetic_small() {
    if !stress_tests_enabled() {
        eprintln!("Skipping stress test (set BR_E2E_STRESS=1 to enable)");
        return;
    }

    let binaries = discover_binaries().expect("Binary discovery failed");

    println!("\n=== Synthetic Scale-Up Benchmark: Small (10K) ===\n");

    let config = SyntheticConfig::from_tier(ScaleTier::Small);
    eprintln!(
        "Generating synthetic dataset ({} issues)...",
        config.issue_count
    );

    let dataset = SyntheticDataset::generate(config, &binaries.br.path)
        .expect("Failed to generate synthetic dataset");

    eprintln!("Running benchmarks...");
    let benchmark = benchmark_synthetic(&dataset, &binaries.br.path);
    print_benchmark(&benchmark);

    // Write results
    let output_dir = PathBuf::from("target/benchmark-results");
    fs::create_dir_all(&output_dir).expect("create output dir");
    let output_path = output_dir.join("synthetic_small_latest.json");
    write_benchmark_json(&[benchmark], &output_path).expect("write results");
    println!("Results written to: {}", output_path.display());
}

/// Medium scale synthetic benchmark (50k issues).
/// Env gate: BR_E2E_STRESS=1
#[test]
#[ignore = "stress test: BR_E2E_STRESS=1 cargo test --test bench_synthetic_scale -- --ignored"]
fn stress_synthetic_medium() {
    if !stress_tests_enabled() {
        eprintln!("Skipping stress test (set BR_E2E_STRESS=1 to enable)");
        return;
    }

    let binaries = discover_binaries().expect("Binary discovery failed");

    println!("\n=== Synthetic Scale-Up Benchmark: Medium (50K) ===\n");

    let config = SyntheticConfig::from_tier(ScaleTier::Medium);
    eprintln!(
        "Generating synthetic dataset ({} issues)...",
        config.issue_count
    );

    let dataset = SyntheticDataset::generate(config, &binaries.br.path)
        .expect("Failed to generate synthetic dataset");

    eprintln!("Running benchmarks...");
    let benchmark = benchmark_synthetic(&dataset, &binaries.br.path);
    print_benchmark(&benchmark);

    // Write results
    let output_dir = PathBuf::from("target/benchmark-results");
    fs::create_dir_all(&output_dir).expect("create output dir");
    let output_path = output_dir.join("synthetic_medium_latest.json");
    write_benchmark_json(&[benchmark], &output_path).expect("write results");
    println!("Results written to: {}", output_path.display());
}

/// Large scale synthetic benchmark (100k issues).
/// Env gate: BR_E2E_STRESS=1
#[test]
#[ignore = "stress test: BR_E2E_STRESS=1 cargo test --test bench_synthetic_scale -- --ignored"]
fn stress_synthetic_large() {
    if !stress_tests_enabled() {
        eprintln!("Skipping stress test (set BR_E2E_STRESS=1 to enable)");
        return;
    }

    let binaries = discover_binaries().expect("Binary discovery failed");

    println!("\n=== Synthetic Scale-Up Benchmark: Large (100K) ===\n");

    let config = SyntheticConfig::from_tier(ScaleTier::Large);
    eprintln!(
        "Generating synthetic dataset ({} issues)...",
        config.issue_count
    );

    let dataset = SyntheticDataset::generate(config, &binaries.br.path)
        .expect("Failed to generate synthetic dataset");

    eprintln!("Running benchmarks...");
    let benchmark = benchmark_synthetic(&dataset, &binaries.br.path);
    print_benchmark(&benchmark);

    // Write results
    let output_dir = PathBuf::from("target/benchmark-results");
    fs::create_dir_all(&output_dir).expect("create output dir");
    let output_path = output_dir.join("synthetic_large_latest.json");
    write_benchmark_json(&[benchmark], &output_path).expect("write results");
    println!("Results written to: {}", output_path.display());
}

/// Extra-large scale synthetic benchmark (250k issues).
/// Env gate: BR_E2E_STRESS=1
#[test]
#[ignore = "stress test: BR_E2E_STRESS=1 cargo test --test bench_synthetic_scale -- --ignored"]
fn stress_synthetic_xlarge() {
    if !stress_tests_enabled() {
        eprintln!("Skipping stress test (set BR_E2E_STRESS=1 to enable)");
        return;
    }

    let binaries = discover_binaries().expect("Binary discovery failed");

    println!("\n=== Synthetic Scale-Up Benchmark: XLarge (250K) ===\n");

    let config = SyntheticConfig::from_tier(ScaleTier::XLarge);
    eprintln!(
        "Generating synthetic dataset ({} issues)...",
        config.issue_count
    );

    let dataset = SyntheticDataset::generate(config, &binaries.br.path)
        .expect("Failed to generate synthetic dataset");

    eprintln!("Running benchmarks...");
    let benchmark = benchmark_synthetic(&dataset, &binaries.br.path);
    print_benchmark(&benchmark);

    // Write results
    let output_dir = PathBuf::from("target/benchmark-results");
    fs::create_dir_all(&output_dir).expect("create output dir");
    let output_path = output_dir.join("synthetic_xlarge_latest.json");
    write_benchmark_json(&[benchmark], &output_path).expect("write results");
    println!("Results written to: {}", output_path.display());
}

/// Run all synthetic benchmarks in sequence.
/// Env gate: BR_E2E_STRESS=1
#[test]
#[ignore = "stress test: BR_E2E_STRESS=1 cargo test --test bench_synthetic_scale -- --ignored"]
fn stress_synthetic_all() {
    if !stress_tests_enabled() {
        eprintln!("Skipping stress test (set BR_E2E_STRESS=1 to enable)");
        return;
    }

    let binaries = discover_binaries().expect("Binary discovery failed");
    let mut all_benchmarks = Vec::new();

    println!("\n=== Synthetic Scale-Up Benchmark Suite ===\n");

    for tier in [ScaleTier::Small, ScaleTier::Medium, ScaleTier::Large] {
        let config = SyntheticConfig::from_tier(tier);
        eprintln!(
            "\n[{}] Generating {} issues...",
            tier.name(),
            config.issue_count
        );

        match SyntheticDataset::generate(config, &binaries.br.path) {
            Ok(dataset) => {
                eprintln!("[{}] Running benchmarks...", tier.name());
                let benchmark = benchmark_synthetic(&dataset, &binaries.br.path);
                print_benchmark(&benchmark);
                all_benchmarks.push(benchmark);
            }
            Err(e) => {
                eprintln!("[{}] FAILED: {e}", tier.name());
            }
        }
    }

    // Write combined results
    if !all_benchmarks.is_empty() {
        let output_dir = PathBuf::from("target/benchmark-results");
        fs::create_dir_all(&output_dir).expect("create output dir");

        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let output_path = output_dir.join(format!("synthetic_all_{timestamp}.json"));
        write_benchmark_json(&all_benchmarks, &output_path).expect("write results");
        println!("\nAll results written to: {}", output_path.display());

        // Also write latest.json
        let latest_path = output_dir.join("synthetic_all_latest.json");
        write_benchmark_json(&all_benchmarks, &latest_path).expect("write latest");
    }

    // Print overall summary
    println!("\n{}", "=".repeat(80));
    println!("OVERALL SUMMARY");
    println!("{}", "=".repeat(80));

    for b in &all_benchmarks {
        let ips = b
            .summary
            .issues_per_second
            .map_or_else(|| "N/A".to_string(), |v| format!("{:.0}", v));
        println!(
            "{}: {}ms total, {} issues/sec for list",
            b.tier, b.summary.total_duration_ms, ips
        );
    }
}

/// Unit test for synthetic config creation.
#[test]
fn test_synthetic_config_from_tier() {
    let config = SyntheticConfig::from_tier(ScaleTier::Large);
    assert_eq!(config.issue_count, 100_000);
    assert!((config.dependency_density - 0.5).abs() < 0.01);
    assert_eq!(config.seed, 42);
}

/// Unit test for scale tier properties.
#[test]
fn test_scale_tier_properties() {
    assert_eq!(ScaleTier::Small.issue_count(), 10_000);
    assert_eq!(ScaleTier::Medium.issue_count(), 50_000);
    assert_eq!(ScaleTier::Large.issue_count(), 100_000);
    assert_eq!(ScaleTier::XLarge.issue_count(), 250_000);

    assert_eq!(ScaleTier::Small.name(), "small_10k");
    assert_eq!(ScaleTier::Large.name(), "large_100k");
}

/// Unit test for title generation.
#[test]
fn test_generate_title() {
    let mut rng = StdRng::seed_from_u64(42);
    let title = generate_title(&mut rng, 123);

    // Should have format "Prefix subject (#123)"
    assert!(title.contains("#123"));
    assert!(title.len() > 10);
}
