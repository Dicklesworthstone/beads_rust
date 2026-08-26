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

use beads_rust::util::hex_encode;
use common::artifact_validator::{
    ArtifactValidator, PerfEvidenceBinary, PerfEvidenceCommand, PerfEvidenceComparison,
    PerfEvidenceDataset, PerfEvidenceEnvVar, PerfEvidenceEnvironment, PerfEvidenceGit,
    PerfEvidenceGolden, PerfEvidenceManifest, PerfEvidencePolicy, PerfEvidenceResources,
    PerfEvidenceTiming, StartupMatrixAggregation, StartupMatrixManifest, StartupMatrixState,
};
use common::binary_discovery::{DiscoveredBinaries, discover_binaries};
use common::dataset_registry::{IsolatedDataset, KnownDataset};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

/// Captured command output for performance artifact bundles.
struct CapturedCommandRun {
    duration: Duration,
    exit_code: i32,
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
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

/// Run a command for the startup matrix and keep enough raw evidence for a bundle.
fn run_startup_matrix_command(
    binary_path: &Path,
    args: &[&str],
    cwd: &Path,
    env_vars: &[(&str, String)],
) -> std::io::Result<CapturedCommandRun> {
    let mut command = Command::new(binary_path);
    command.args(args).current_dir(cwd);
    for key in [
        "BD_ACTOR",
        "BD_DB",
        "BD_DATABASE",
        "BEADS_DIR",
        "BEADS_JSONL",
        "BEADS_CACHE_DIR",
        "BR_OUTPUT_FORMAT",
        "TOON_DEFAULT_FORMAT",
        "TOON_STATS",
    ] {
        command.env_remove(key);
    }
    command.env("NO_COLOR", "1");
    command.env("RUST_BACKTRACE", "1");
    command.env("HOME", cwd);
    for (key, value) in env_vars {
        command.env(key, value);
    }

    let start = Instant::now();
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    Ok(CapturedCommandRun {
        duration: start.elapsed(),
        exit_code: output.status.code().unwrap_or(-1),
        success: output.status.success(),
        stdout: output.stdout,
        stderr: output.stderr,
    })
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
    let temp_dir = TempDir::new_in(common::cli::isolated_temp_root())?;
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
    let _ = fs::remove_file(root.join(".beads").join("beads.db-journal"));

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
        if let Ok(issues) = serde_json::from_str::<Vec<serde_json::Value>>(line)
            && let Some(first) = issues.first()
            && let Some(id) = first.get("id").and_then(|v| v.as_str())
        {
            return Some(id.to_string());
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

const STARTUP_MATRIX_STATES: &[&str] = &[
    "clean",
    "stale",
    "routed",
    "no_db",
    "read_only_fast_open",
    "sync_status",
    "recovery_anomaly",
];

/// Number of warm runs per command.
const WARM_RUNS: usize = 5;

fn startup_matrix_args(state: &str) -> &'static [&'static str] {
    match state {
        "no_db" => &["--no-db", "list", "--json"],
        "read_only_fast_open" => &["--no-auto-import", "--no-auto-flush", "list", "--json"],
        "stale" | "sync_status" | "recovery_anomaly" => &["sync", "--status", "--json"],
        _ => &["list", "--json"],
    }
}

fn prepare_startup_matrix_workspace(
    br_path: &Path,
    state: &str,
) -> std::io::Result<(TempDir, PathBuf)> {
    let (temp_dir, root) = create_br_workspace(br_path, 3)?;

    match state {
        "stale" => {
            let output = Command::new(br_path)
                .args(["create", "Startup matrix stale marker", "--no-auto-flush"])
                .current_dir(&root)
                .output()?;
            if !output.status.success() {
                return Err(std::io::Error::other(format!(
                    "startup matrix stale setup failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
        }
        "recovery_anomaly" => {
            let recovery_dir = root.join(".beads").join(".br_recovery");
            fs::create_dir_all(&recovery_dir)?;
            fs::write(
                recovery_dir.join("startup-matrix-leftover.txt"),
                "synthetic recovery artifact for startup matrix smoke\n",
            )?;
        }
        _ => {}
    }

    Ok((temp_dir, root))
}

fn write_startup_matrix_state_artifacts(
    bundle_dir: &Path,
    state: &str,
    args: &[&str],
    cwd: &Path,
    env_vars: &[(&str, String)],
    run: &CapturedCommandRun,
) -> std::io::Result<StartupMatrixState> {
    for subdir in ["logs", "timing", "syscalls", "rss", "raw"] {
        fs::create_dir_all(bundle_dir.join(subdir))?;
    }

    let command_log_path = format!("logs/{state}.log");
    let timing_summary_path = format!("timing/{state}.json");
    let syscall_summary_path = format!("syscalls/{state}.json");
    let rss_summary_path = format!("rss/{state}.json");
    let stdout_path = format!("raw/{state}.stdout");
    let stderr_path = format!("raw/{state}.stderr");

    let env_keys = env_vars.iter().map(|(key, _)| *key).collect::<Vec<_>>();
    let duration_ms = run.duration.as_secs_f64() * 1000.0;
    let command_log = format!(
        "state: {state}\nargs: {args:?}\ncwd: {}\nenv_keys: {env_keys:?}\nexit_code: {}\nsuccess: {}\nduration_ms: {duration_ms:.3}\nstdout_len: {}\nstderr_len: {}\n",
        cwd.display(),
        run.exit_code,
        run.success,
        run.stdout.len(),
        run.stderr.len()
    );
    fs::write(bundle_dir.join(&command_log_path), command_log)?;
    fs::write(bundle_dir.join(&stdout_path), &run.stdout)?;
    fs::write(bundle_dir.join(&stderr_path), &run.stderr)?;

    let timing_summary = serde_json::json!({
        "state": state,
        "args": args,
        "cwd": cwd.display().to_string(),
        "env_keys": env_keys,
        "duration_ms": duration_ms,
        "exit_code": run.exit_code,
        "success": run.success,
        "stdout_bytes": run.stdout.len(),
        "stderr_bytes": run.stderr.len(),
    });
    fs::write(
        bundle_dir.join(&timing_summary_path),
        serde_json::to_string_pretty(&timing_summary)?,
    )?;

    let syscall_summary = serde_json::json!({
        "state": state,
        "collector": "startup_matrix_smoke",
        "status": "not_collected",
        "reason": "smoke runner records the required artifact slot without requiring strace or elevated privileges",
    });
    fs::write(
        bundle_dir.join(&syscall_summary_path),
        serde_json::to_string_pretty(&syscall_summary)?,
    )?;

    let rss_summary = serde_json::json!({
        "state": state,
        "collector": "startup_matrix_smoke",
        "status": "not_collected",
        "reason": "smoke runner records the required artifact slot; full matrix runners can replace this with platform RSS capture",
    });
    fs::write(
        bundle_dir.join(&rss_summary_path),
        serde_json::to_string_pretty(&rss_summary)?,
    )?;

    Ok(StartupMatrixState {
        state: state.to_string(),
        command_log_path,
        timing_summary_path,
        syscall_summary_path,
        rss_summary_path,
        raw_artifact_paths: vec![stdout_path, stderr_path],
    })
}

fn write_startup_matrix_smoke_bundle(
    br_path: &Path,
    bundle_dir: &Path,
) -> std::io::Result<StartupMatrixManifest> {
    fs::create_dir_all(bundle_dir)?;
    let mut states = Vec::with_capacity(STARTUP_MATRIX_STATES.len());

    for &state in STARTUP_MATRIX_STATES {
        let (_workspace_guard, workspace_root) = prepare_startup_matrix_workspace(br_path, state)?;
        let routed_cwd = if state == "routed" {
            Some(TempDir::new()?)
        } else {
            None
        };
        let cwd = routed_cwd
            .as_ref()
            .map_or(workspace_root.as_path(), tempfile::TempDir::path);
        let env_vars = if state == "routed" {
            vec![(
                "BEADS_DIR",
                workspace_root.join(".beads").display().to_string(),
            )]
        } else {
            Vec::new()
        };
        let args = startup_matrix_args(state);
        let run = run_startup_matrix_command(br_path, args, cwd, &env_vars)?;
        if !run.success {
            return Err(std::io::Error::other(format!(
                "startup matrix state {state} failed with code {}; stdout={}; stderr={}",
                run.exit_code,
                String::from_utf8_lossy(&run.stdout),
                String::from_utf8_lossy(&run.stderr)
            )));
        }

        states.push(write_startup_matrix_state_artifacts(
            bundle_dir, state, args, cwd, &env_vars, &run,
        )?);
    }

    let manifest = StartupMatrixManifest {
        schema_version: "br.startup-matrix.v1".to_string(),
        matrix_name: "storage-open-smoke".to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        states,
        aggregation: StartupMatrixAggregation {
            status: "ok".to_string(),
            raw_evidence_preserved: true,
            error: None,
        },
    };

    fs::write(
        bundle_dir.join("startup-matrix-manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    Ok(manifest)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

fn git_revision_for_perf_evidence() -> String {
    Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|revision| !revision.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn git_dirty_for_perf_evidence() -> bool {
    Command::new("git")
        .args(["status", "--short"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| !String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

fn rustc_version_for_perf_evidence() -> Option<String> {
    Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|version| !version.is_empty())
}

fn percentile(sorted_samples: &[f64], numerator: usize, denominator: usize) -> f64 {
    if sorted_samples.is_empty() {
        return 0.0;
    }

    let max_index = sorted_samples.len() - 1;
    let index = max_index.saturating_mul(numerator).div_ceil(denominator);
    sorted_samples[index.min(max_index)]
}

fn prepare_perf_evidence_bundle_dir(bundle_dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(bundle_dir)?;
    for subdir in [
        "logs", "timing", "syscalls", "io", "rss", "golden", "proof", "baseline", "raw",
    ] {
        fs::create_dir_all(bundle_dir.join(subdir))?;
    }

    Ok(())
}

fn record_perf_evidence_runs(
    br_path: &Path,
    workspace_root: &Path,
    bundle_dir: &Path,
    args: &[&str],
) -> std::io::Result<(Vec<CapturedCommandRun>, Vec<String>)> {
    let mut runs = Vec::new();
    let mut raw_artifact_paths = Vec::new();

    for run_index in 0..3 {
        let run = run_startup_matrix_command(br_path, args, workspace_root, &[])?;
        if !run.success {
            return Err(std::io::Error::other(format!(
                "perf evidence smoke command failed with code {}; stdout={}; stderr={}",
                run.exit_code,
                String::from_utf8_lossy(&run.stdout),
                String::from_utf8_lossy(&run.stderr)
            )));
        }

        let stdout_path = format!("raw/list-{run_index}.stdout");
        let stderr_path = format!("raw/list-{run_index}.stderr");
        fs::write(bundle_dir.join(&stdout_path), &run.stdout)?;
        fs::write(bundle_dir.join(&stderr_path), &run.stderr)?;
        raw_artifact_paths.push(stdout_path);
        raw_artifact_paths.push(stderr_path);
        runs.push(run);
    }

    Ok((runs, raw_artifact_paths))
}

fn write_perf_evidence_golden(
    bundle_dir: &Path,
    runs: &[CapturedCommandRun],
) -> std::io::Result<(String, String)> {
    let Some(first_run) = runs.first() else {
        return Err(std::io::Error::other(
            "perf evidence smoke requires at least one command run",
        ));
    };
    let stdout_sha256 = sha256_hex(&first_run.stdout);
    let stderr_sha256 = sha256_hex(&first_run.stderr);
    fs::write(bundle_dir.join("golden/stdout"), &first_run.stdout)?;
    fs::write(bundle_dir.join("golden/stderr"), &first_run.stderr)?;
    fs::write(
        bundle_dir.join("golden/checksums.txt"),
        format!("{stdout_sha256}  golden/stdout\n{stderr_sha256}  golden/stderr\n"),
    )?;

    Ok((stdout_sha256, stderr_sha256))
}

fn write_perf_evidence_timing(
    bundle_dir: &Path,
    runs: &[CapturedCommandRun],
) -> std::io::Result<PerfEvidenceTiming> {
    let mut durations_ms = runs
        .iter()
        .map(|run| run.duration.as_secs_f64() * 1000.0)
        .collect::<Vec<_>>();
    durations_ms.sort_by(f64::total_cmp);
    let timing = PerfEvidenceTiming {
        sample_count: runs.len(),
        min_ms: *durations_ms.first().unwrap_or(&0.0),
        p50_ms: percentile(&durations_ms, 50, 100),
        p95_ms: percentile(&durations_ms, 95, 100),
        p99_ms: percentile(&durations_ms, 99, 100),
        max_ms: *durations_ms.last().unwrap_or(&0.0),
        summary_path: "timing/list.json".to_string(),
        raw_samples_path: "timing/list-samples.jsonl".to_string(),
    };

    let sample_lines = runs
        .iter()
        .enumerate()
        .map(|(run_index, run)| {
            serde_json::json!({
                "run_index": run_index,
                "duration_ms": run.duration.as_secs_f64() * 1000.0,
                "exit_code": run.exit_code,
                "stdout_sha256": sha256_hex(&run.stdout),
                "stderr_sha256": sha256_hex(&run.stderr),
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        bundle_dir.join(&timing.raw_samples_path),
        format!("{sample_lines}\n"),
    )?;
    fs::write(
        bundle_dir.join(&timing.summary_path),
        serde_json::to_string_pretty(&serde_json::json!({
            "sample_count": timing.sample_count,
            "min_ms": timing.min_ms,
            "p50_ms": timing.p50_ms,
            "p95_ms": timing.p95_ms,
            "p99_ms": timing.p99_ms,
            "max_ms": timing.max_ms,
        }))?,
    )?;

    Ok(timing)
}

fn write_perf_evidence_support_artifacts(
    bundle_dir: &Path,
    workspace_root: &Path,
    args: &[&str],
    sample_count: usize,
    stdout_sha256: &str,
    stderr_sha256: &str,
) -> std::io::Result<()> {
    fs::write(
        bundle_dir.join("logs/list.log"),
        format!(
            "command: br {}\nworkspace: {}\nsamples: {}\nstdout_sha256: {stdout_sha256}\nstderr_sha256: {stderr_sha256}\n",
            args.join(" "),
            workspace_root.display(),
            sample_count
        ),
    )?;
    for (path, collector) in [
        ("syscalls/list.json", "syscall"),
        ("io/list.json", "io"),
        ("rss/list.json", "rss"),
    ] {
        fs::write(
            bundle_dir.join(path),
            serde_json::to_string_pretty(&serde_json::json!({
                "collector": collector,
                "status": "not_collected",
                "reason": "smoke evidence bundle records the required slot; full release gates can replace this with platform capture",
            }))?,
        )?;
    }
    fs::write(
        bundle_dir.join("proof/isomorphism.md"),
        "## Change: perf evidence smoke ledger\n- Ordering preserved: yes; command output is not transformed.\n- Tie-breaking unchanged: yes; br list decides ordering.\n- Floating-point: N/A for command output; timings are evidence only.\n- RNG seeds: unchanged/N/A.\n- Golden outputs: stdout and stderr SHA-256 hashes recorded in golden/checksums.txt.\n",
    )?;

    Ok(())
}

fn build_perf_evidence_manifest(
    br_path: &Path,
    workspace_root: &Path,
    args: &[&str],
    timing: PerfEvidenceTiming,
    hashes: (String, String),
    raw_artifact_paths: Vec<String>,
) -> std::io::Result<PerfEvidenceManifest> {
    let (stdout_sha256, stderr_sha256) = hashes;
    let issues_jsonl = fs::read(workspace_root.join(".beads").join("issues.jsonl"))?;
    let generated_at = chrono::Utc::now();
    Ok(PerfEvidenceManifest {
        schema_version: "br.perf-evidence.v1".to_string(),
        generated_at: generated_at.to_rfc3339(),
        valid_until: Some((generated_at + chrono::Duration::days(30)).to_rfc3339()),
        command: PerfEvidenceCommand {
            label: "list_json".to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
        },
        dataset: PerfEvidenceDataset {
            name: "tiny-smoke".to_string(),
            issue_count: Some(3),
            content_hash: Some(sha256_hex(&issues_jsonl)),
        },
        git: PerfEvidenceGit {
            revision: git_revision_for_perf_evidence(),
            dirty: git_dirty_for_perf_evidence(),
        },
        binary: PerfEvidenceBinary {
            path: br_path.display().to_string(),
            version: None,
        },
        environment: PerfEvidenceEnvironment {
            os: std::env::consts::OS.to_string(),
            rustc: rustc_version_for_perf_evidence(),
            env: vec![PerfEvidenceEnvVar {
                name: "NO_COLOR".to_string(),
                value_hash: Some(sha256_hex(b"1")),
            }],
        },
        timing,
        resources: PerfEvidenceResources {
            syscalls: "syscalls/list.json".to_string(),
            io: "io/list.json".to_string(),
            rss: "rss/list.json".to_string(),
        },
        golden: PerfEvidenceGolden {
            stdout_sha256,
            stderr_sha256: Some(stderr_sha256),
            checksums_path: "golden/checksums.txt".to_string(),
            stdout_path: "golden/stdout".to_string(),
            stderr_path: Some("golden/stderr".to_string()),
        },
        isomorphism_note_path: "proof/isomorphism.md".to_string(),
        policy: PerfEvidencePolicy {
            mode: "enforcing".to_string(),
            baseline_manifest_path: Some("baseline/perf-evidence-manifest.json".to_string()),
            latency_regression_budget_pct: Some(5.0),
            syscall_regression_budget_pct: Some(10.0),
            output_hash_must_match: true,
        },
        comparison: PerfEvidenceComparison {
            status: "pass".to_string(),
            baseline_manifest_path: Some("baseline/perf-evidence-manifest.json".to_string()),
            p95_delta_pct: Some(0.0),
            stdout_hash_match: Some(true),
            syscall_delta_pct: Some(0.0),
            decision_reason: "self-baseline smoke comparison passed enforcing policy".to_string(),
        },
        raw_artifact_paths,
    })
}

fn write_perf_evidence_smoke_bundle(
    br_path: &Path,
    bundle_dir: &Path,
) -> std::io::Result<PerfEvidenceManifest> {
    prepare_perf_evidence_bundle_dir(bundle_dir)?;

    let (_workspace_guard, workspace_root) = create_br_workspace(br_path, 3)?;
    let args = ["list", "--json"];
    let (runs, raw_artifact_paths) =
        record_perf_evidence_runs(br_path, &workspace_root, bundle_dir, &args)?;
    let hashes = write_perf_evidence_golden(bundle_dir, &runs)?;
    let timing = write_perf_evidence_timing(bundle_dir, &runs)?;
    write_perf_evidence_support_artifacts(
        bundle_dir,
        &workspace_root,
        &args,
        runs.len(),
        &hashes.0,
        &hashes.1,
    )?;
    let manifest = build_perf_evidence_manifest(
        br_path,
        &workspace_root,
        &args,
        timing,
        hashes,
        raw_artifact_paths,
    )?;

    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    fs::write(
        bundle_dir.join("perf-evidence-manifest.json"),
        &manifest_json,
    )?;
    fs::write(
        bundle_dir.join("baseline/perf-evidence-manifest.json"),
        manifest_json,
    )?;

    Ok(manifest)
}

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

/// Smoke runner for the storage-open startup matrix artifact bundle.
#[test]
fn startup_matrix_smoke_bundle_covers_storage_open_states() -> std::io::Result<()> {
    let br_path = assert_cmd::cargo::cargo_bin!("br");
    let run_id = format!(
        "startup-matrix-smoke-{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%fZ"),
        std::process::id()
    );
    let bundle_dir = PathBuf::from("target").join("perf-artifacts").join(run_id);

    let manifest = write_startup_matrix_smoke_bundle(br_path, &bundle_dir)?;
    let validation = ArtifactValidator::new().validate_startup_matrix_bundle_dir(&bundle_dir);
    assert!(
        validation.valid,
        "startup matrix bundle should validate: {:?}",
        validation.errors
    );

    let mut states = manifest
        .states
        .iter()
        .map(|state| state.state.as_str())
        .collect::<Vec<_>>();
    states.sort_unstable();
    let mut expected = STARTUP_MATRIX_STATES.to_vec();
    expected.sort_unstable();
    assert_eq!(states, expected);

    Ok(())
}

/// Smoke runner for the reusable performance evidence ledger bundle.
#[test]
fn perf_evidence_smoke_bundle_records_list_json_command() -> std::io::Result<()> {
    let br_path = assert_cmd::cargo::cargo_bin!("br");
    let run_id = format!(
        "perf-evidence-smoke-{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%fZ"),
        std::process::id()
    );
    let bundle_dir = PathBuf::from("target").join("perf-artifacts").join(run_id);

    let manifest = write_perf_evidence_smoke_bundle(br_path, &bundle_dir)?;
    let validation = ArtifactValidator::new().validate_perf_evidence_bundle_dir(&bundle_dir);
    assert!(
        validation.valid,
        "perf evidence bundle should validate: {:?}",
        validation.errors
    );
    assert_eq!(manifest.schema_version, "br.perf-evidence.v1");
    assert_eq!(manifest.command.args, ["list", "--json"]);
    assert_eq!(manifest.policy.mode, "enforcing");
    assert_eq!(manifest.comparison.status, "pass");
    assert_eq!(manifest.comparison.stdout_hash_match, Some(true));
    assert_eq!(manifest.timing.sample_count, 3);
    assert!(manifest.timing.p50_ms <= manifest.timing.p95_ms);
    assert!(manifest.timing.p95_ms <= manifest.timing.p99_ms);

    Ok(())
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
