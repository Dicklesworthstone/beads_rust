//! E2E Test Harness Foundation
//!
//! Provides unified infrastructure for all E2E, conformance, and benchmark tests:
//! - `TestWorkspace`: Isolated temp workspace with git init, `.beads/` setup
//! - `CommandRunner`: Execute br/bd with env isolation, capture all outputs
//! - `ArtifactLogger`: JSONL event log, stdout/stderr capture, file tree snapshots

#![allow(clippy::similar_names)]

use assert_cmd::Command;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use walkdir::WalkDir;

fn br_binary_path() -> PathBuf {
    assert_cmd::cargo::cargo_bin!("br").to_path_buf()
}

/// Get the path to the bd (Go beads) binary.
/// Checks `BD_BINARY` environment variable first, falls back to "bd" for PATH lookup.
fn bd_binary_path() -> String {
    std::env::var("BD_BINARY").unwrap_or_else(|_| "bd".to_string())
}

/// Global mutex for artifact logging to prevent interleaving
fn artifact_mutex() -> &'static Mutex<()> {
    static MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    MUTEX.get_or_init(|| Mutex::new(()))
}

/// Result of running a command
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub success: bool,
    pub duration: Duration,
    pub log_path: PathBuf,
    /// Whether stdout was truncated due to guardrails
    pub stdout_truncated: bool,
    /// Whether stderr was truncated due to guardrails
    pub stderr_truncated: bool,
    /// Whether command timed out
    pub timed_out: bool,
}

impl CommandResult {
    pub fn assert_success(&self) {
        assert!(
            self.success,
            "Command failed (exit {})\nstdout: {}\nstderr: {}",
            self.exit_code, self.stdout, self.stderr
        );
    }

    pub fn assert_failure(&self) {
        assert!(
            !self.success,
            "Command succeeded unexpectedly\nstdout: {}\nstderr: {}",
            self.stdout, self.stderr
        );
    }

    pub fn stdout_contains(&self, needle: &str) -> bool {
        self.stdout.contains(needle)
    }

    pub fn stderr_contains(&self, needle: &str) -> bool {
        self.stderr.contains(needle)
    }

    /// Parse stdout as JSON, extracting the JSON payload from potential prefix text
    pub fn json<T: for<'de> serde::Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        let payload = extract_json_payload(&self.stdout);
        serde_json::from_str(&payload)
    }
}

/// JSONL event entry for artifact logging
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RunEvent {
    pub timestamp: String,
    pub event_type: String,
    pub label: String,
    pub binary: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub exit_code: i32,
    pub success: bool,
    pub duration_ms: u128,
    pub stdout_len: usize,
    pub stderr_len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_path: Option<String>,
}

/// File tree snapshot entry
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Configuration for artifact logging
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct ArtifactConfig {
    pub enabled: bool,
    pub capture_stdout: bool,
    pub capture_stderr: bool,
    pub capture_snapshots: bool,
    pub preserve_on_success: bool,
}

impl Default for ArtifactConfig {
    fn default() -> Self {
        Self {
            enabled: std::env::var("HARNESS_ARTIFACTS").is_ok_and(|v| v == "1"),
            capture_stdout: true,
            capture_stderr: true,
            capture_snapshots: true,
            preserve_on_success: std::env::var("HARNESS_PRESERVE_SUCCESS").is_ok_and(|v| v == "1"),
        }
    }
}

// ============================================================================
// RUNNER POLICY (beads_rust-enep)
// ============================================================================

/// Parallelism mode for scenario execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ParallelismMode {
    /// Run scenarios one at a time (safest, default)
    #[default]
    Serial,
    /// Run scenarios in parallel with configurable worker count
    Parallel,
}

/// Resource guardrails for test execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceGuardrails {
    /// Maximum stdout capture size in bytes (truncates if exceeded)
    pub max_stdout_bytes: usize,
    /// Maximum stderr capture size in bytes (truncates if exceeded)
    pub max_stderr_bytes: usize,
    /// Maximum artifact file size in bytes (skips if exceeded)
    pub max_artifact_file_bytes: usize,
    /// Maximum total artifact directory size in bytes
    pub max_artifact_dir_bytes: usize,
    /// Number of days to retain artifacts (0 = indefinite)
    pub artifact_retention_days: u32,
    /// Maximum log file lines (0 = unlimited)
    pub max_log_lines: usize,
}

impl Default for ResourceGuardrails {
    fn default() -> Self {
        Self {
            // 1MB stdout/stderr max (prevents memory issues)
            max_stdout_bytes: parse_env_usize("HARNESS_MAX_STDOUT_BYTES", 1024 * 1024),
            max_stderr_bytes: parse_env_usize("HARNESS_MAX_STDERR_BYTES", 1024 * 1024),
            // 10MB per artifact file
            max_artifact_file_bytes: parse_env_usize(
                "HARNESS_MAX_ARTIFACT_BYTES",
                10 * 1024 * 1024,
            ),
            // 100MB total per test
            max_artifact_dir_bytes: parse_env_usize(
                "HARNESS_MAX_ARTIFACT_DIR_BYTES",
                100 * 1024 * 1024,
            ),
            // 7 days retention by default
            artifact_retention_days: parse_env_u32("HARNESS_ARTIFACT_RETENTION_DAYS", 7),
            // 10000 log lines max
            max_log_lines: parse_env_usize("HARNESS_MAX_LOG_LINES", 10000),
        }
    }
}

/// Execution policy for the test runner.
///
/// Controls timeouts, parallelism, and resource limits.
/// Configurable via environment variables for CI/local flexibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerPolicy {
    /// Timeout for individual command execution
    pub command_timeout: Duration,
    /// Timeout for entire scenario (setup + test + teardown)
    pub scenario_timeout: Duration,
    /// Parallelism mode (serial or parallel)
    pub parallelism_mode: ParallelismMode,
    /// Number of parallel workers (only used in Parallel mode)
    pub max_parallel_workers: usize,
    /// Resource guardrails
    pub guardrails: ResourceGuardrails,
    /// Fail fast on first error (stop remaining scenarios)
    pub fail_fast: bool,
    /// Retry failed scenarios this many times
    pub retry_count: u32,
    /// Skip slow scenarios (tagged "slow") unless explicitly enabled
    pub skip_slow: bool,
}

impl Default for RunnerPolicy {
    fn default() -> Self {
        Self {
            // 30 second command timeout by default
            command_timeout: Duration::from_secs(parse_env_u64("HARNESS_COMMAND_TIMEOUT_SECS", 30)),
            // 5 minute scenario timeout by default
            scenario_timeout: Duration::from_secs(parse_env_u64(
                "HARNESS_SCENARIO_TIMEOUT_SECS",
                300,
            )),
            // Serial by default for safety
            parallelism_mode: if std::env::var("HARNESS_PARALLEL").is_ok_and(|v| v == "1") {
                ParallelismMode::Parallel
            } else {
                ParallelismMode::Serial
            },
            // Default to number of CPUs, capped at 8
            max_parallel_workers: parse_env_usize(
                "HARNESS_PARALLEL_WORKERS",
                std::thread::available_parallelism()
                    .map(|p| p.get().min(8))
                    .unwrap_or(4),
            ),
            guardrails: ResourceGuardrails::default(),
            fail_fast: std::env::var("HARNESS_FAIL_FAST").is_ok_and(|v| v == "1"),
            retry_count: parse_env_u32("HARNESS_RETRY_COUNT", 0),
            skip_slow: std::env::var("HARNESS_SKIP_SLOW").is_ok_and(|v| v == "1"),
        }
    }
}

impl RunnerPolicy {
    /// Create a strict policy with tight timeouts (for CI)
    pub fn ci() -> Self {
        Self {
            command_timeout: Duration::from_secs(10),
            scenario_timeout: Duration::from_secs(60),
            parallelism_mode: ParallelismMode::Serial,
            max_parallel_workers: 1,
            guardrails: ResourceGuardrails::default(),
            fail_fast: true,
            retry_count: 0,
            skip_slow: true,
        }
    }

    /// Create a relaxed policy with generous timeouts (for local dev)
    pub fn local() -> Self {
        Self {
            command_timeout: Duration::from_secs(60),
            scenario_timeout: Duration::from_secs(600),
            parallelism_mode: ParallelismMode::Serial,
            max_parallel_workers: 4,
            guardrails: ResourceGuardrails::default(),
            fail_fast: false,
            retry_count: 1,
            skip_slow: false,
        }
    }

    /// Create a benchmark policy with no timeouts
    pub fn benchmark() -> Self {
        Self {
            command_timeout: Duration::from_secs(3600),  // 1 hour
            scenario_timeout: Duration::from_secs(7200), // 2 hours
            parallelism_mode: ParallelismMode::Serial,   // Serial for accurate timing
            max_parallel_workers: 1,
            guardrails: ResourceGuardrails {
                max_stdout_bytes: 100 * 1024 * 1024, // 100MB
                max_stderr_bytes: 100 * 1024 * 1024,
                ..Default::default()
            },
            fail_fast: false,
            retry_count: 0,
            skip_slow: false,
        }
    }

    /// Builder: set command timeout
    pub const fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    /// Builder: set scenario timeout
    pub const fn with_scenario_timeout(mut self, timeout: Duration) -> Self {
        self.scenario_timeout = timeout;
        self
    }

    /// Builder: set parallelism mode
    pub const fn with_parallelism(mut self, mode: ParallelismMode, workers: usize) -> Self {
        self.parallelism_mode = mode;
        self.max_parallel_workers = workers;
        self
    }

    /// Builder: set fail fast
    pub const fn with_fail_fast(mut self, fail_fast: bool) -> Self {
        self.fail_fast = fail_fast;
        self
    }

    /// Builder: set retry count
    pub const fn with_retry(mut self, count: u32) -> Self {
        self.retry_count = count;
        self
    }

    /// Convert to JSON for logging in summary
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "command_timeout_secs": self.command_timeout.as_secs(),
            "scenario_timeout_secs": self.scenario_timeout.as_secs(),
            "parallelism_mode": self.parallelism_mode,
            "max_parallel_workers": self.max_parallel_workers,
            "fail_fast": self.fail_fast,
            "retry_count": self.retry_count,
            "skip_slow": self.skip_slow,
            "guardrails": {
                "max_stdout_bytes": self.guardrails.max_stdout_bytes,
                "max_stderr_bytes": self.guardrails.max_stderr_bytes,
                "max_artifact_file_bytes": self.guardrails.max_artifact_file_bytes,
                "max_artifact_dir_bytes": self.guardrails.max_artifact_dir_bytes,
                "artifact_retention_days": self.guardrails.artifact_retention_days,
                "max_log_lines": self.guardrails.max_log_lines,
            }
        })
    }
}

/// Parse an environment variable as usize, with default
fn parse_env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Parse an environment variable as u32, with default
fn parse_env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Parse an environment variable as u64, with default
fn parse_env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Truncate a string to max bytes, appending truncation notice
fn truncate_output(output: &str, max_bytes: usize) -> (String, bool) {
    if output.len() <= max_bytes {
        return (output.to_string(), false);
    }

    // Find a safe UTF-8 boundary
    let mut truncate_at = max_bytes.saturating_sub(100); // Leave room for notice
    while truncate_at > 0 && !output.is_char_boundary(truncate_at) {
        truncate_at -= 1;
    }

    let truncated = format!(
        "{}\n\n[TRUNCATED: Output exceeded {} bytes limit, {} bytes total]",
        &output[..truncate_at],
        max_bytes,
        output.len()
    );
    (truncated, true)
}

/// Artifact logger for a test run
pub struct ArtifactLogger {
    suite: String,
    test: String,
    artifact_dir: PathBuf,
    events_path: PathBuf,
    config: ArtifactConfig,
    run_count: usize,
}

impl ArtifactLogger {
    pub fn new(suite: &str, test: &str) -> Self {
        let artifact_dir = PathBuf::from("target/test-artifacts")
            .join(suite)
            .join(test);
        let config = ArtifactConfig::default();

        if config.enabled {
            fs::create_dir_all(&artifact_dir).ok();
        }

        let events_path = artifact_dir.join("events.jsonl");

        Self {
            suite: suite.to_string(),
            test: test.to_string(),
            artifact_dir,
            events_path,
            config,
            run_count: 0,
        }
    }

    pub fn with_config(mut self, config: ArtifactConfig) -> Self {
        self.config = config;
        if self.config.enabled {
            fs::create_dir_all(&self.artifact_dir).ok();
        }
        self
    }

    pub fn log_command(
        &mut self,
        label: &str,
        binary: &str,
        args: &[String],
        cwd: &Path,
        result: &CommandResult,
    ) {
        if !self.config.enabled {
            return;
        }

        self.run_count += 1;
        let run_id = format!("{:04}_{}", self.run_count, label);

        let stdout_path = if self.config.capture_stdout && !result.stdout.is_empty() {
            let path = self.artifact_dir.join(format!("{run_id}.stdout"));
            fs::write(&path, &result.stdout).ok();
            Some(path.display().to_string())
        } else {
            None
        };

        let stderr_path = if self.config.capture_stderr && !result.stderr.is_empty() {
            let path = self.artifact_dir.join(format!("{run_id}.stderr"));
            fs::write(&path, &result.stderr).ok();
            Some(path.display().to_string())
        } else {
            None
        };

        let event = RunEvent {
            timestamp: Utc::now().to_rfc3339(),
            event_type: "command".to_string(),
            label: label.to_string(),
            binary: binary.to_string(),
            args: args.to_vec(),
            cwd: cwd.display().to_string(),
            exit_code: result.exit_code,
            success: result.success,
            duration_ms: result.duration.as_millis(),
            stdout_len: result.stdout.len(),
            stderr_len: result.stderr.len(),
            stdout_path,
            stderr_path,
            snapshot_path: None,
        };

        self.append_event(&event);
    }

    pub fn log_snapshot(&self, label: &str, workspace_root: &Path) {
        if !self.config.enabled || !self.config.capture_snapshots {
            return;
        }

        let snapshot_path = self.artifact_dir.join(format!("{label}.snapshot.json"));
        let entries = collect_file_tree(workspace_root);

        if let Ok(json) = serde_json::to_string_pretty(&entries) {
            fs::write(&snapshot_path, json).ok();
        }

        let event = RunEvent {
            timestamp: Utc::now().to_rfc3339(),
            event_type: "snapshot".to_string(),
            label: label.to_string(),
            binary: String::new(),
            args: vec![],
            cwd: workspace_root.display().to_string(),
            exit_code: 0,
            success: true,
            duration_ms: 0,
            stdout_len: 0,
            stderr_len: 0,
            stdout_path: None,
            stderr_path: None,
            snapshot_path: Some(snapshot_path.display().to_string()),
        };

        self.append_event(&event);
    }

    fn append_event(&self, event: &RunEvent) {
        let _guard = artifact_mutex().lock().expect("artifact mutex");

        if let Ok(file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events_path)
        {
            let mut writer = BufWriter::new(file);
            if let Ok(json) = serde_json::to_string(event) {
                let _ = writeln!(writer, "{json}");
            }
        }
    }

    pub fn write_summary(&self, passed: bool) {
        self.write_summary_with_policy(passed, None);
    }

    /// Write summary with optional policy information
    pub fn write_summary_with_policy(&self, passed: bool, policy: Option<&RunnerPolicy>) {
        if !self.config.enabled {
            return;
        }

        let mut summary = serde_json::json!({
            "suite": self.suite,
            "test": self.test,
            "passed": passed,
            "run_count": self.run_count,
            "timestamp": Utc::now().to_rfc3339(),
        });

        // Include policy if provided
        if let Some(p) = policy {
            summary["policy"] = p.to_json();
        }

        let summary_path = self.artifact_dir.join("summary.json");
        if let Ok(json) = serde_json::to_string_pretty(&summary) {
            fs::write(summary_path, json).ok();
        }

        if passed && !self.config.preserve_on_success {
            // Clean up detailed artifacts on success, keep only summary
            if let Ok(entries) = fs::read_dir(&self.artifact_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path
                        .extension()
                        .is_some_and(|e| e == "stdout" || e == "stderr")
                    {
                        fs::remove_file(path).ok();
                    }
                }
            }
        }
    }
}

/// Collect file tree entries for a directory
fn collect_file_tree(root: &Path) -> Vec<FileEntry> {
    let mut entries = Vec::new();

    for entry in WalkDir::new(root)
        .max_depth(5)
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
            entries.push(FileEntry {
                path: rel_str,
                size: metadata.as_ref().map_or(0, std::fs::Metadata::len),
                is_dir: entry.file_type().is_dir(),
            });
        }
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// E2E test workspace with isolated temp directory
pub struct TestWorkspace {
    pub temp_dir: TempDir,
    pub root: PathBuf,
    pub beads_dir: PathBuf,
    logger: ArtifactLogger,
    git_initialized: bool,
}

impl TestWorkspace {
    /// Create a new test workspace
    pub fn new(suite: &str, test: &str) -> Self {
        let temp_dir = TempDir::new().expect("create temp dir");
        let root = temp_dir.path().to_path_buf();
        let beads_dir = root.join(".beads");
        let logger = ArtifactLogger::new(suite, test);

        Self {
            temp_dir,
            root,
            beads_dir,
            logger,
            git_initialized: false,
        }
    }

    /// Initialize git in the workspace
    pub fn init_git(&mut self) -> &mut Self {
        if !self.git_initialized {
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&self.root)
                .output()
                .expect("git init");

            std::process::Command::new("git")
                .args(["config", "user.email", "test@test.local"])
                .current_dir(&self.root)
                .output()
                .ok();

            std::process::Command::new("git")
                .args(["config", "user.name", "Test"])
                .current_dir(&self.root)
                .output()
                .ok();

            self.git_initialized = true;
        }
        self
    }

    /// Initialize br in the workspace
    pub fn init_br(&mut self) -> CommandResult {
        self.run_br(["init"], "init")
    }

    /// Run br command
    pub fn run_br<I, S>(&mut self, args: I, label: &str) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_binary("br", args, label)
    }

    /// Run br command with environment variables
    pub fn run_br_env<I, S, E, K, V>(&mut self, args: I, env_vars: E, label: &str) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.run_binary_env("br", args, env_vars, label)
    }

    /// Run br command with stdin input
    pub fn run_br_stdin<I, S>(&mut self, args: I, input: &str, label: &str) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_binary_stdin("br", args, input, label)
    }

    /// Run br command with environment variables and stdin input
    pub fn run_br_env_stdin<I, S, E, K, V>(
        &mut self,
        args: I,
        env_vars: E,
        input: &str,
        label: &str,
    ) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.run_binary_full("br", args, env_vars, Some(input), label)
    }

    /// Run bd (Go beads) command
    /// Respects `BD_BINARY` environment variable for custom binary path
    pub fn run_bd<I, S>(&mut self, args: I, label: &str) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_system_binary(&bd_binary_path(), args, label)
    }

    /// Run any binary from the cargo build
    fn run_binary<I, S>(&mut self, binary: &str, args: I, label: &str) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_binary_full(
            binary,
            args,
            std::iter::empty::<(String, String)>(),
            None,
            label,
        )
    }

    fn run_binary_env<I, S, E, K, V>(
        &mut self,
        binary: &str,
        args: I,
        env_vars: E,
        label: &str,
    ) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.run_binary_full(binary, args, env_vars, None, label)
    }

    fn run_binary_stdin<I, S>(
        &mut self,
        binary: &str,
        args: I,
        input: &str,
        label: &str,
    ) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_binary_full(
            binary,
            args,
            std::iter::empty::<(String, String)>(),
            Some(input),
            label,
        )
    }

    fn run_binary_full<I, S, E, K, V>(
        &mut self,
        binary: &str,
        args: I,
        env_vars: E,
        stdin_input: Option<&str>,
        label: &str,
    ) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let bin_path = br_binary_path();
        let mut cmd = Command::new(&bin_path);
        cmd.current_dir(&self.root);

        let args_vec: Vec<String> = args
            .into_iter()
            .map(|a| a.as_ref().to_string_lossy().to_string())
            .collect();
        cmd.args(&args_vec);

        cmd.envs(env_vars);
        cmd.env("NO_COLOR", "1");
        cmd.env("RUST_LOG", "beads_rust=debug");
        cmd.env("RUST_BACKTRACE", "1");
        cmd.env("HOME", &self.root);

        if let Some(input) = stdin_input {
            cmd.write_stdin(input);
        }

        let start = Instant::now();
        let output = cmd.output().expect("run command");
        let duration = start.elapsed();

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        let log_path = self.root.join("logs").join(format!("{label}.log"));
        fs::create_dir_all(log_path.parent().unwrap()).ok();

        let log_content = format!(
            "label: {}\nbinary: {}\nargs: {:?}\ncwd: {}\nexit_code: {}\nduration: {:?}\n\n--- stdout ---\n{}\n\n--- stderr ---\n{}",
            label,
            binary,
            args_vec,
            self.root.display(),
            exit_code,
            duration,
            stdout,
            stderr
        );
        fs::write(&log_path, &log_content).ok();

        let result = CommandResult {
            stdout,
            stderr,
            exit_code,
            success: output.status.success(),
            duration,
            log_path,
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        };

        self.logger
            .log_command(label, binary, &args_vec, &self.root, &result);

        result
    }

    /// Run a system binary (e.g., bd, git)
    fn run_system_binary<I, S>(&mut self, binary: &str, args: I, label: &str) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = std::process::Command::new(binary);
        cmd.current_dir(&self.root);

        let args_vec: Vec<String> = args
            .into_iter()
            .map(|a| a.as_ref().to_string_lossy().to_string())
            .collect();
        cmd.args(&args_vec);

        cmd.env("NO_COLOR", "1");
        cmd.env("HOME", &self.root);

        let start = Instant::now();
        let output = cmd.output().unwrap_or_else(|_| panic!("run {binary}"));
        let duration = start.elapsed();

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        let log_path = self.root.join("logs").join(format!("{label}.log"));
        fs::create_dir_all(log_path.parent().unwrap()).ok();

        let log_content = format!(
            "label: {}\nbinary: {}\nargs: {:?}\ncwd: {}\nexit_code: {}\nduration: {:?}\n\n--- stdout ---\n{}\n\n--- stderr ---\n{}",
            label,
            binary,
            args_vec,
            self.root.display(),
            exit_code,
            duration,
            stdout,
            stderr
        );
        fs::write(&log_path, &log_content).ok();

        let result = CommandResult {
            stdout,
            stderr,
            exit_code,
            success: output.status.success(),
            duration,
            log_path,
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        };

        self.logger
            .log_command(label, binary, &args_vec, &self.root, &result);

        result
    }

    /// Take a snapshot of the current file tree
    pub fn snapshot(&self, label: &str) {
        self.logger.log_snapshot(label, &self.root);
    }

    /// Finalize the test, writing summary
    pub fn finish(self, passed: bool) {
        self.logger.write_summary(passed);
    }

    /// Write summary without consuming self (for tests that need to continue using workspace)
    pub fn write_summary(&self, passed: bool) {
        self.logger.write_summary(passed);
    }

    /// Read a file from the workspace
    pub fn read_file(&self, rel_path: &str) -> Option<String> {
        fs::read_to_string(self.root.join(rel_path)).ok()
    }

    /// Write a file to the workspace
    pub fn write_file(&self, rel_path: &str, content: &str) {
        let path = self.root.join(rel_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(path, content).expect("write file");
    }

    /// Check if a file exists in the workspace
    pub fn file_exists(&self, rel_path: &str) -> bool {
        self.root.join(rel_path).exists()
    }

    /// List files in a directory
    pub fn list_dir(&self, rel_path: &str) -> Vec<String> {
        let path = self.root.join(rel_path);
        fs::read_dir(&path)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect()
    }
}

/// Conformance workspace with paired br/bd directories
pub struct ConformanceWorkspace {
    pub temp_dir: TempDir,
    pub br_workspace: PathBuf,
    pub bd_workspace: PathBuf,
    pub log_dir: PathBuf,
    logger: ArtifactLogger,
}

impl ConformanceWorkspace {
    pub fn new(suite: &str, test: &str) -> Self {
        let temp_dir = TempDir::new().expect("create temp dir");
        let root = temp_dir.path().to_path_buf();
        let br_workspace = root.join("br_workspace");
        let bd_workspace = root.join("bd_workspace");
        let log_dir = root.join("logs");
        let logger = ArtifactLogger::new(suite, test);

        fs::create_dir_all(&br_workspace).expect("create br workspace");
        fs::create_dir_all(&bd_workspace).expect("create bd workspace");
        fs::create_dir_all(&log_dir).expect("create log dir");

        Self {
            temp_dir,
            br_workspace,
            bd_workspace,
            log_dir,
            logger,
        }
    }

    /// Initialize both workspaces
    pub fn init_both(&mut self) -> (CommandResult, CommandResult) {
        let br_result = self.run_br(["init"], "init");
        let bd_result = self.run_bd(["init"], "init");
        (br_result, bd_result)
    }

    /// Run br command
    pub fn run_br<I, S>(&mut self, args: I, label: &str) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_in_workspace(
            "br",
            &self.br_workspace.clone(),
            args,
            &format!("br_{label}"),
        )
    }

    /// Run bd command
    /// Respects `BD_BINARY` environment variable for custom binary path
    pub fn run_bd<I, S>(&mut self, args: I, label: &str) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_in_workspace_system(
            &bd_binary_path(),
            &self.bd_workspace.clone(),
            args,
            &format!("bd_{label}"),
        )
    }

    /// Run br command with environment variables
    pub fn run_br_env<I, S, E, K, V>(&mut self, args: I, env_vars: E, label: &str) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.run_in_workspace_env(
            "br",
            &self.br_workspace.clone(),
            args,
            env_vars,
            None,
            &format!("br_{label}"),
        )
    }

    /// Run br command with stdin input
    pub fn run_br_stdin<I, S>(&mut self, args: I, input: &str, label: &str) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_in_workspace_env(
            "br",
            &self.br_workspace.clone(),
            args,
            std::iter::empty::<(String, String)>(),
            Some(input),
            &format!("br_{label}"),
        )
    }

    /// Run br command with env vars and stdin
    pub fn run_br_env_stdin<I, S, E, K, V>(
        &mut self,
        args: I,
        env_vars: E,
        input: &str,
        label: &str,
    ) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.run_in_workspace_env(
            "br",
            &self.br_workspace.clone(),
            args,
            env_vars,
            Some(input),
            &format!("br_{label}"),
        )
    }

    /// Run bd command with environment variables
    /// Respects `BD_BINARY` environment variable for custom binary path
    pub fn run_bd_env<I, S, E, K, V>(&mut self, args: I, env_vars: E, label: &str) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.run_in_workspace_system_env(
            &bd_binary_path(),
            &self.bd_workspace.clone(),
            args,
            env_vars,
            None,
            &format!("bd_{label}"),
        )
    }

    /// Run bd command with stdin input
    /// Respects `BD_BINARY` environment variable for custom binary path
    pub fn run_bd_stdin<I, S>(&mut self, args: I, input: &str, label: &str) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_in_workspace_system_env(
            &bd_binary_path(),
            &self.bd_workspace.clone(),
            args,
            std::iter::empty::<(String, String)>(),
            Some(input),
            &format!("bd_{label}"),
        )
    }

    /// Run bd command with env vars and stdin
    /// Respects `BD_BINARY` environment variable for custom binary path
    pub fn run_bd_env_stdin<I, S, E, K, V>(
        &mut self,
        args: I,
        env_vars: E,
        input: &str,
        label: &str,
    ) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.run_in_workspace_system_env(
            &bd_binary_path(),
            &self.bd_workspace.clone(),
            args,
            env_vars,
            Some(input),
            &format!("bd_{label}"),
        )
    }

    #[allow(clippy::ptr_arg)]
    fn run_in_workspace<I, S>(
        &mut self,
        binary: &str,
        cwd: &PathBuf,
        args: I,
        label: &str,
    ) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let bin_path = br_binary_path();
        let mut cmd = Command::new(&bin_path);
        cmd.current_dir(cwd);

        let args_vec: Vec<String> = args
            .into_iter()
            .map(|a| a.as_ref().to_string_lossy().to_string())
            .collect();
        cmd.args(&args_vec);

        cmd.env("NO_COLOR", "1");
        cmd.env("RUST_LOG", "beads_rust=debug");
        cmd.env("RUST_BACKTRACE", "1");
        cmd.env("HOME", cwd);

        let start = Instant::now();
        let output = cmd.output().expect("run command");
        let duration = start.elapsed();

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        let log_path = self.log_dir.join(format!("{label}.log"));
        let log_content = format!(
            "label: {}\nbinary: {}\nargs: {:?}\ncwd: {}\nexit_code: {}\nduration: {:?}\n\n--- stdout ---\n{}\n\n--- stderr ---\n{}",
            label,
            binary,
            args_vec,
            cwd.display(),
            exit_code,
            duration,
            stdout,
            stderr
        );
        fs::write(&log_path, &log_content).ok();

        let result = CommandResult {
            stdout,
            stderr,
            exit_code,
            success: output.status.success(),
            duration,
            log_path,
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        };

        self.logger
            .log_command(label, binary, &args_vec, cwd, &result);

        result
    }

    #[allow(clippy::ptr_arg)]
    fn run_in_workspace_env<I, S, E, K, V>(
        &mut self,
        binary: &str,
        cwd: &PathBuf,
        args: I,
        env_vars: E,
        stdin_input: Option<&str>,
        label: &str,
    ) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let bin_path = br_binary_path();
        let mut cmd = Command::new(&bin_path);
        cmd.current_dir(cwd);

        let args_vec: Vec<String> = args
            .into_iter()
            .map(|a| a.as_ref().to_string_lossy().to_string())
            .collect();
        cmd.args(&args_vec);

        cmd.envs(env_vars);
        cmd.env("NO_COLOR", "1");
        cmd.env("RUST_LOG", "beads_rust=debug");
        cmd.env("RUST_BACKTRACE", "1");
        cmd.env("HOME", cwd);

        if let Some(input) = stdin_input {
            cmd.write_stdin(input);
        }

        let start = Instant::now();
        let output = cmd.output().expect("run command");
        let duration = start.elapsed();

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        let log_path = self.log_dir.join(format!("{label}.log"));
        let log_content = format!(
            "label: {}\nbinary: {}\nargs: {:?}\ncwd: {}\nexit_code: {}\nduration: {:?}\n\n--- stdout ---\n{}\n\n--- stderr ---\n{}",
            label,
            binary,
            args_vec,
            cwd.display(),
            exit_code,
            duration,
            stdout,
            stderr
        );
        fs::write(&log_path, &log_content).ok();

        let result = CommandResult {
            stdout,
            stderr,
            exit_code,
            success: output.status.success(),
            duration,
            log_path,
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        };

        self.logger
            .log_command(label, binary, &args_vec, cwd, &result);

        result
    }

    #[allow(clippy::ptr_arg)]
    fn run_in_workspace_system<I, S>(
        &mut self,
        binary: &str,
        cwd: &PathBuf,
        args: I,
        label: &str,
    ) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = std::process::Command::new(binary);
        cmd.current_dir(cwd);

        let args_vec: Vec<String> = args
            .into_iter()
            .map(|a| a.as_ref().to_string_lossy().to_string())
            .collect();
        cmd.args(&args_vec);

        cmd.env("NO_COLOR", "1");
        cmd.env("HOME", cwd);

        let start = Instant::now();
        let output = cmd.output().unwrap_or_else(|_| panic!("run {binary}"));
        let duration = start.elapsed();

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        let log_path = self.log_dir.join(format!("{label}.log"));
        let log_content = format!(
            "label: {}\nbinary: {}\nargs: {:?}\ncwd: {}\nexit_code: {}\nduration: {:?}\n\n--- stdout ---\n{}\n\n--- stderr ---\n{}",
            label,
            binary,
            args_vec,
            cwd.display(),
            exit_code,
            duration,
            stdout,
            stderr
        );
        fs::write(&log_path, &log_content).ok();

        let result = CommandResult {
            stdout,
            stderr,
            exit_code,
            success: output.status.success(),
            duration,
            log_path,
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        };

        self.logger
            .log_command(label, binary, &args_vec, cwd, &result);

        result
    }

    #[allow(clippy::ptr_arg)]
    fn run_in_workspace_system_env<I, S, E, K, V>(
        &mut self,
        binary: &str,
        cwd: &PathBuf,
        args: I,
        env_vars: E,
        stdin_input: Option<&str>,
        label: &str,
    ) -> CommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let mut cmd = std::process::Command::new(binary);
        cmd.current_dir(cwd);

        let args_vec: Vec<String> = args
            .into_iter()
            .map(|a| a.as_ref().to_string_lossy().to_string())
            .collect();
        cmd.args(&args_vec);

        cmd.envs(env_vars);
        cmd.env("NO_COLOR", "1");
        cmd.env("HOME", cwd);

        let mut build_result = |output: std::process::Output, duration: Duration| {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let exit_code = output.status.code().unwrap_or(-1);

            let log_path = self.log_dir.join(format!("{label}.log"));
            let log_content = format!(
                "label: {}\nbinary: {}\nargs: {:?}\ncwd: {}\nexit_code: {}\nduration: {:?}\n\n--- stdout ---\n{}\n\n--- stderr ---\n{}",
                label,
                binary,
                args_vec,
                cwd.display(),
                exit_code,
                duration,
                stdout,
                stderr
            );
            fs::write(&log_path, &log_content).ok();

            let result = CommandResult {
                stdout,
                stderr,
                exit_code,
                success: output.status.success(),
                duration,
                log_path,
                stdout_truncated: false,
                stderr_truncated: false,
                timed_out: false,
            };

            self.logger
                .log_command(label, binary, &args_vec, cwd, &result);

            result
        };

        if let Some(input) = stdin_input {
            use std::io::Write as _;
            cmd.stdin(std::process::Stdio::piped());
            let start = Instant::now();
            let mut child = cmd.spawn().expect("run command");
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(input.as_bytes());
            }
            let output = child.wait_with_output().expect("run command");
            let duration = start.elapsed();
            return build_result(output, duration);
        }

        let start = Instant::now();
        let output = cmd.output().unwrap_or_else(|_| panic!("run {binary}"));
        let duration = start.elapsed();
        build_result(output, duration)
    }

    /// Finalize the test
    pub fn finish(self, passed: bool) {
        self.logger.write_summary(passed);
    }
}

/// Extract JSON payload from stdout (skips non-JSON preamble)
pub fn extract_json_payload(stdout: &str) -> String {
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

/// Parse the created ID from br create output
pub fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    // Handle both formats: "Created bd-xxx: title" and "✓ Created bd-xxx: title"
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_basic() {
        let mut ws = TestWorkspace::new("harness", "test_workspace_basic");
        let result = ws.init_br();
        result.assert_success();
        assert!(ws.file_exists(".beads/beads.db"));
        ws.finish(true);
    }

    #[test]
    fn test_extract_json_payload() {
        let stdout = "Some preamble\n{\"key\": \"value\"}";
        let json = extract_json_payload(stdout);
        assert_eq!(json, "{\"key\": \"value\"}");
    }

    #[test]
    fn test_parse_created_id() {
        let stdout = "Created bd-abc123: Test issue\n";
        let id = parse_created_id(stdout);
        assert_eq!(id, "bd-abc123");
    }

    // ========================================================================
    // RunnerPolicy tests (beads_rust-enep)
    // ========================================================================

    #[test]
    fn test_runner_policy_default() {
        let policy = RunnerPolicy::default();

        // Verify default timeouts
        assert_eq!(policy.command_timeout.as_secs(), 30);
        assert_eq!(policy.scenario_timeout.as_secs(), 300);

        // Verify default parallelism (serial)
        assert_eq!(policy.parallelism_mode, ParallelismMode::Serial);

        // Verify default guardrails
        assert_eq!(policy.guardrails.max_stdout_bytes, 1024 * 1024);
        assert_eq!(policy.guardrails.max_stderr_bytes, 1024 * 1024);
        assert_eq!(policy.guardrails.artifact_retention_days, 7);
    }

    #[test]
    fn test_runner_policy_ci() {
        let policy = RunnerPolicy::ci();

        // CI has tighter timeouts
        assert_eq!(policy.command_timeout.as_secs(), 10);
        assert_eq!(policy.scenario_timeout.as_secs(), 60);

        // CI uses fail_fast
        assert!(policy.fail_fast);
        assert!(policy.skip_slow);
    }

    #[test]
    fn test_runner_policy_local() {
        let policy = RunnerPolicy::local();

        // Local has relaxed timeouts
        assert_eq!(policy.command_timeout.as_secs(), 60);
        assert_eq!(policy.scenario_timeout.as_secs(), 600);

        // Local doesn't fail fast
        assert!(!policy.fail_fast);
        assert!(!policy.skip_slow);
        assert_eq!(policy.retry_count, 1);
    }

    #[test]
    fn test_runner_policy_benchmark() {
        let policy = RunnerPolicy::benchmark();

        // Benchmark has very long timeouts
        assert_eq!(policy.command_timeout.as_secs(), 3600);
        assert_eq!(policy.scenario_timeout.as_secs(), 7200);

        // Benchmark uses serial for accurate timing
        assert_eq!(policy.parallelism_mode, ParallelismMode::Serial);
        assert_eq!(policy.max_parallel_workers, 1);

        // Benchmark has larger output buffers
        assert_eq!(policy.guardrails.max_stdout_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn test_runner_policy_builder() {
        let policy = RunnerPolicy::default()
            .with_command_timeout(Duration::from_secs(15))
            .with_scenario_timeout(Duration::from_secs(120))
            .with_parallelism(ParallelismMode::Parallel, 4)
            .with_fail_fast(true)
            .with_retry(2);

        assert_eq!(policy.command_timeout.as_secs(), 15);
        assert_eq!(policy.scenario_timeout.as_secs(), 120);
        assert_eq!(policy.parallelism_mode, ParallelismMode::Parallel);
        assert_eq!(policy.max_parallel_workers, 4);
        assert!(policy.fail_fast);
        assert_eq!(policy.retry_count, 2);
    }

    #[test]
    fn test_runner_policy_to_json() {
        let policy = RunnerPolicy::ci();
        let json = policy.to_json();

        assert_eq!(json["command_timeout_secs"], 10);
        assert_eq!(json["scenario_timeout_secs"], 60);
        assert_eq!(json["fail_fast"], true);
        assert!(json["guardrails"].is_object());
    }

    #[test]
    fn test_resource_guardrails_default() {
        let guardrails = ResourceGuardrails::default();

        assert_eq!(guardrails.max_stdout_bytes, 1024 * 1024);
        assert_eq!(guardrails.max_stderr_bytes, 1024 * 1024);
        assert_eq!(guardrails.max_artifact_file_bytes, 10 * 1024 * 1024);
        assert_eq!(guardrails.max_artifact_dir_bytes, 100 * 1024 * 1024);
        assert_eq!(guardrails.artifact_retention_days, 7);
        assert_eq!(guardrails.max_log_lines, 10000);
    }

    #[test]
    fn test_truncate_output() {
        // Short output - no truncation
        let (result, truncated) = truncate_output("short", 100);
        assert_eq!(result, "short");
        assert!(!truncated);

        // Long output - truncated
        let long_output = "x".repeat(1000);
        let (result, truncated) = truncate_output(&long_output, 200);
        assert!(truncated);
        assert!(result.len() < long_output.len());
        assert!(result.contains("[TRUNCATED:"));
    }

    #[test]
    fn test_parallelism_mode_default() {
        let mode = ParallelismMode::default();
        assert_eq!(mode, ParallelismMode::Serial);
    }

    #[test]
    fn test_collect_file_tree_deterministic_order() {
        let temp_dir = TempDir::new().expect("temp dir");
        let root = temp_dir.path();

        fs::create_dir_all(root.join("b_dir")).expect("create b_dir");
        fs::create_dir_all(root.join("a_dir")).expect("create a_dir");
        fs::write(root.join("b_dir/file_b.txt"), "b").expect("write file_b");
        fs::write(root.join("a_dir/file_a.txt"), "a").expect("write file_a");

        let entries = collect_file_tree(root);
        let paths: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();

        assert!(paths.contains(&"a_dir".to_string()));
        assert!(paths.contains(&"a_dir/file_a.txt".to_string()));
        assert!(paths.contains(&"b_dir".to_string()));
        assert!(paths.contains(&"b_dir/file_b.txt".to_string()));

        for window in paths.windows(2) {
            assert!(
                window[0] <= window[1],
                "paths not sorted: {} > {}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn test_artifact_logger_writes_and_cleans() {
        let suite = format!("harness_logger_{}", std::process::id());
        let test = "writes_and_cleans";
        let artifact_dir = PathBuf::from("target/test-artifacts")
            .join(&suite)
            .join(test);

        let config = ArtifactConfig {
            enabled: true,
            capture_stdout: true,
            capture_stderr: true,
            capture_snapshots: false,
            preserve_on_success: false,
        };

        let mut logger = ArtifactLogger::new(&suite, test).with_config(config);

        let result = CommandResult {
            stdout: "stdout data".to_string(),
            stderr: "stderr data".to_string(),
            exit_code: 0,
            success: true,
            duration: Duration::from_millis(5),
            log_path: PathBuf::from("dummy.log"),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        };

        logger.log_command(
            "sample",
            "br",
            &["--version".to_string()],
            Path::new("."),
            &result,
        );

        let events_path = artifact_dir.join("events.jsonl");
        assert!(events_path.exists(), "events.jsonl not written");
        let events = fs::read_to_string(&events_path).expect("read events.jsonl");
        assert!(events.contains("\"event_type\":\"command\""));

        let has_stdout = fs::read_dir(&artifact_dir)
            .expect("read artifact dir")
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|e| e == "stdout"));
        let has_stderr = fs::read_dir(&artifact_dir)
            .expect("read artifact dir")
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|e| e == "stderr"));
        assert!(has_stdout, "stdout artifact missing");
        assert!(has_stderr, "stderr artifact missing");

        logger.write_summary(true);
        assert!(artifact_dir.join("summary.json").exists());

        let has_stdout = fs::read_dir(&artifact_dir)
            .expect("read artifact dir")
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|e| e == "stdout"));
        let has_stderr = fs::read_dir(&artifact_dir)
            .expect("read artifact dir")
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|e| e == "stderr"));
        assert!(!has_stdout, "stdout artifacts not cleaned on success");
        assert!(!has_stderr, "stderr artifacts not cleaned on success");
    }

    #[test]
    fn test_artifact_logger_snapshot_writes_event() {
        let suite = format!("harness_logger_snapshot_{}", std::process::id());
        let test = "snapshot_event";
        let artifact_dir = PathBuf::from("target/test-artifacts")
            .join(&suite)
            .join(test);

        let config = ArtifactConfig {
            enabled: true,
            capture_stdout: false,
            capture_stderr: false,
            capture_snapshots: true,
            preserve_on_success: true,
        };

        let logger = ArtifactLogger::new(&suite, test).with_config(config);

        let temp_dir = TempDir::new().expect("temp dir");
        fs::write(temp_dir.path().join("file.txt"), "content").expect("write file");

        logger.log_snapshot("snapshot", temp_dir.path());

        let snapshot_path = artifact_dir.join("snapshot.snapshot.json");
        assert!(snapshot_path.exists(), "snapshot file missing");

        let events_path = artifact_dir.join("events.jsonl");
        let events = fs::read_to_string(&events_path).expect("read events.jsonl");
        assert!(events.contains("\"event_type\":\"snapshot\""));
    }

    #[test]
    fn test_run_br_env_uses_override() {
        let mut ws = TestWorkspace::new("harness", "run_br_env_override");
        let init = ws.init_br();
        init.assert_success();

        let beads_dir = ws.beads_dir.clone();
        let override_value = beads_dir.to_string_lossy().to_string();

        let result = ws.run_br_env(
            ["where", "--json"],
            [("BEADS_DIR", override_value)],
            "where_env",
        );
        result.assert_success();

        let payload = extract_json_payload(&result.stdout);
        let value: serde_json::Value = serde_json::from_str(&payload).expect("parse where json");
        let path = value.get("path").and_then(|p| p.as_str()).unwrap_or("");

        let expected = beads_dir.canonicalize().unwrap_or(beads_dir);
        assert_eq!(path, expected.to_string_lossy());

        ws.finish(true);
    }
}
