//! Dataset registry for E2E, conformance, and benchmark tests.
//!
//! Provides access to real `.beads` directories as fixtures, with safe copy
//! to isolated temp workspaces. Source datasets are NEVER mutated.

#![allow(dead_code)]

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;

/// Metadata about a dataset for logging and benchmarking.
#[derive(Debug, Clone)]
pub struct DatasetMetadata {
    pub name: String,
    pub source_path: PathBuf,
    pub issue_count: usize,
    pub jsonl_size_bytes: u64,
    pub db_size_bytes: u64,
    pub dependency_count: usize,
    pub content_hash: String,
    pub copied_at: Option<SystemTime>,
    pub copy_duration: Option<Duration>,
    /// Git commit hash of the source repository (if available)
    pub source_commit: Option<String>,
    /// Whether the source was an override (custom path) vs known dataset
    pub is_override: bool,
    /// Override reason/description (if `is_override` is true)
    pub override_reason: Option<String>,
}

impl DatasetMetadata {
    /// Serialize metadata to JSON for inclusion in summary.json.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "source_path": self.source_path.display().to_string(),
            "issue_count": self.issue_count,
            "jsonl_size_bytes": self.jsonl_size_bytes,
            "db_size_bytes": self.db_size_bytes,
            "dependency_count": self.dependency_count,
            "content_hash": self.content_hash,
            "copied_at": self.copied_at.map(|t| format!("{t:?}")),
            "copy_duration_ms": self.copy_duration.map(|d| d.as_millis()),
            "source_commit": self.source_commit,
            "is_override": self.is_override,
            "override_reason": self.override_reason,
        })
    }
}

const SOURCE_COMMIT_OVERRIDE_ENV: &str = "BR_DATASET_SOURCE_COMMIT";
const SOURCE_COMMIT_OVERRIDE_ENVS: &[&str] = &[
    SOURCE_COMMIT_OVERRIDE_ENV,
    "RCH_SOURCE_COMMIT",
    "RCH_GIT_SHA",
    "RCH_GIT_COMMIT",
    "GIT_COMMIT",
    "GITHUB_SHA",
    "CI_COMMIT_SHA",
    "BUILDKITE_COMMIT",
    "DRONE_COMMIT_SHA",
    "CIRCLE_SHA1",
    "VERCEL_GIT_COMMIT_SHA",
];
const WORKSPACE_FAILURE_FIXTURE_DIR: &str = "tests/fixtures/workspace_failures";

/// Known datasets for testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KnownDataset {
    BeadsRust,
    BeadsViewer,
    CodingAgentSessionSearch,
    BrennerBot,
}

impl KnownDataset {
    pub const fn name(self) -> &'static str {
        match self {
            Self::BeadsRust => "beads_rust",
            Self::BeadsViewer => "beads_viewer",
            Self::CodingAgentSessionSearch => "coding_agent_session_search",
            Self::BrennerBot => "brenner_bot",
        }
    }

    pub fn source_path(self) -> PathBuf {
        match self {
            // Use CARGO_MANIFEST_DIR for BeadsRust since we're running from within the repo
            Self::BeadsRust => PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            Self::BeadsViewer => PathBuf::from("/data/projects/beads_viewer"),
            Self::CodingAgentSessionSearch => {
                PathBuf::from("/data/projects/coding_agent_session_search")
            }
            Self::BrennerBot => PathBuf::from("/data/projects/brenner_bot"),
        }
    }

    pub fn beads_dir(self) -> PathBuf {
        self.source_path().join(".beads")
    }

    pub const fn all() -> &'static [Self] {
        &[
            Self::BeadsRust,
            Self::BeadsViewer,
            Self::CodingAgentSessionSearch,
            Self::BrennerBot,
        ]
    }
}

/// A registry that manages dataset fixtures for tests.
pub struct DatasetRegistry {
    datasets: HashMap<String, DatasetMetadata>,
    source_hashes: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceFailureCommandOutcome {
    Success,
    SuccessWithAutoRecovery,
    DoctorClean,
    ReportsErrors,
    RepairApplied,
    RepairNoop,
    StatusInSync,
    StatusJsonlNewer,
    StatusDiverged,
    StatusDbNewer,
    FailsPrefixMismatch,
    FailsConflictMarkers,
    FailsInvalidJson,
    FailsRepeatedRepair,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct WorkspaceFailureCommandExpectation {
    pub surface: String,
    pub outcome: WorkspaceFailureCommandOutcome,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct WorkspaceFailureFixtureMetadata {
    pub name: String,
    pub family: String,
    pub description: String,
    pub expected_classification: String,
    pub expected_command_outcomes: Vec<WorkspaceFailureCommandExpectation>,
    pub source_hint: String,
    pub notes: Vec<String>,
}

impl WorkspaceFailureFixtureMetadata {
    pub fn outcome_for(&self, surface: &str) -> Option<WorkspaceFailureCommandOutcome> {
        self.expected_command_outcomes
            .iter()
            .find(|expectation| expectation.surface == surface)
            .map(|expectation| expectation.outcome)
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceFailureFixture {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub metadata: WorkspaceFailureFixtureMetadata,
}

pub struct IsolatedWorkspaceFailureFixture {
    pub temp_dir: TempDir,
    pub root: PathBuf,
    pub beads_dir: PathBuf,
    pub fixture: WorkspaceFailureFixture,
}

impl WorkspaceFailureFixture {
    fn load_from_root(root: PathBuf) -> std::io::Result<Self> {
        let manifest_path = root.join("fixture.json");
        let manifest = fs::read_to_string(&manifest_path)?;
        let metadata = serde_json::from_str(&manifest).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid fixture.json for {}: {err}", root.display()),
            )
        })?;

        Ok(Self {
            root,
            manifest_path,
            metadata,
        })
    }
}

impl IsolatedWorkspaceFailureFixture {
    pub fn workspace_root(&self) -> &Path {
        &self.root
    }
}

fn workspace_failure_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(WORKSPACE_FAILURE_FIXTURE_DIR)
}

pub fn list_workspace_failure_fixtures() -> std::io::Result<Vec<WorkspaceFailureFixture>> {
    let fixture_root = workspace_failure_fixture_root();
    if !fixture_root.exists() {
        return Ok(Vec::new());
    }

    let mut fixtures = Vec::new();
    for entry in fs::read_dir(fixture_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        fixtures.push(WorkspaceFailureFixture::load_from_root(entry.path())?);
    }

    fixtures.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
    Ok(fixtures)
}

pub fn isolated_workspace_failure_fixture(
    name: &str,
) -> std::io::Result<IsolatedWorkspaceFailureFixture> {
    let fixture = list_workspace_failure_fixtures()?
        .into_iter()
        .find(|fixture| fixture.metadata.name == name)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Workspace failure fixture '{name}' not found"),
            )
        })?;

    let temp_dir = TempDir::new()?;
    let root = temp_dir.path().to_path_buf();
    copy_workspace_failure_fixture_root(&fixture.root, &root)?;

    Ok(IsolatedWorkspaceFailureFixture {
        temp_dir,
        root: root.clone(),
        beads_dir: root.join(".beads"),
        fixture,
    })
}

impl DatasetRegistry {
    /// Create a new registry, scanning available datasets.
    pub fn new() -> Self {
        let mut registry = Self {
            datasets: HashMap::new(),
            source_hashes: HashMap::new(),
        };

        for dataset in KnownDataset::all() {
            if let Ok(metadata) = Self::scan_dataset(*dataset) {
                registry
                    .source_hashes
                    .insert(dataset.name().to_string(), metadata.content_hash.clone());
                registry
                    .datasets
                    .insert(dataset.name().to_string(), metadata);
            }
        }

        registry
    }

    /// Check if a dataset is available (exists and has valid .beads).
    pub fn is_available(&self, dataset: KnownDataset) -> bool {
        self.datasets.contains_key(dataset.name())
    }

    /// Get metadata for a dataset.
    pub fn metadata(&self, dataset: KnownDataset) -> Option<&DatasetMetadata> {
        self.datasets.get(dataset.name())
    }

    /// List all available datasets.
    pub fn available_datasets(&self) -> Vec<&DatasetMetadata> {
        self.datasets.values().collect()
    }

    /// Scan a dataset and compute its metadata.
    fn scan_dataset(dataset: KnownDataset) -> std::io::Result<DatasetMetadata> {
        let beads_dir = dataset.beads_dir();
        if !beads_dir.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "Dataset {} not found at {}",
                    dataset.name(),
                    beads_dir.display()
                ),
            ));
        }

        let jsonl_path = beads_dir.join("issues.jsonl");
        let db_path = beads_dir.join("beads.db");

        // Require beads.db to exist (not committed to git, only present in dev environments)
        if !db_path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "Dataset {} missing beads.db at {} (only available in dev environment)",
                    dataset.name(),
                    db_path.display()
                ),
            ));
        }

        let jsonl_size_bytes = fs::metadata(&jsonl_path).map_or(0, |m| m.len());
        let db_size_bytes = fs::metadata(&db_path).map_or(0, |m| m.len());

        let issue_count = count_jsonl_lines(&jsonl_path).unwrap_or(0);
        let dependency_count = count_dependencies(&jsonl_path).unwrap_or(0);

        let content_hash = hash_beads_directory(&beads_dir)?;

        // Get git commit from source repository (if .git exists)
        let source_commit = get_git_commit(&dataset.source_path());

        Ok(DatasetMetadata {
            name: dataset.name().to_string(),
            source_path: dataset.source_path(),
            issue_count,
            jsonl_size_bytes,
            db_size_bytes,
            dependency_count,
            content_hash,
            copied_at: None,
            copy_duration: None,
            source_commit,
            is_override: false,
            override_reason: None,
        })
    }

    /// Verify source dataset hasn't changed since registry creation.
    pub fn verify_source_integrity(&self, dataset: KnownDataset) -> Result<(), String> {
        let Some(original_hash) = self.source_hashes.get(dataset.name()) else {
            return Err(format!("Dataset {} not in registry", dataset.name()));
        };

        let current_hash = hash_beads_directory(&dataset.beads_dir())
            .map_err(|e| format!("Failed to hash {}: {e}", dataset.name()))?;

        if &current_hash != original_hash {
            return Err(format!(
                "Source dataset {} has been mutated! Original: {}, Current: {}",
                dataset.name(),
                original_hash,
                current_hash
            ));
        }

        Ok(())
    }
}

impl Default for DatasetRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A copied dataset in an isolated temp workspace.
pub struct IsolatedDataset {
    pub temp_dir: TempDir,
    pub root: PathBuf,
    pub beads_dir: PathBuf,
    pub metadata: DatasetMetadata,
    pub source_dataset: KnownDataset,
}

impl IsolatedDataset {
    /// Create an isolated copy of a dataset.
    ///
    /// # Safety
    /// - Source dataset is read-only; only the temp copy is writable.
    /// - Copies .beads directory and creates minimal repo scaffold.
    pub fn from_dataset(dataset: KnownDataset) -> std::io::Result<Self> {
        let source_beads = dataset.beads_dir();
        if !source_beads.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Dataset {} not found", dataset.name()),
            ));
        }

        let start = Instant::now();
        let temp_dir = TempDir::new()?;
        let root = temp_dir.path().to_path_buf();
        let beads_dir = root.join(".beads");

        // Copy .beads directory
        copy_dir_recursive(&source_beads, &beads_dir)?;

        // Create minimal repo scaffold (empty .git marker, not a real git repo)
        fs::create_dir_all(root.join(".git"))?;
        fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n")?;

        let copy_duration = start.elapsed();

        // Scan copied dataset for metadata
        let jsonl_path = beads_dir.join("issues.jsonl");
        let db_path = beads_dir.join("beads.db");

        let jsonl_size_bytes = fs::metadata(&jsonl_path).map_or(0, |m| m.len());
        let db_size_bytes = fs::metadata(&db_path).map_or(0, |m| m.len());
        let issue_count = count_jsonl_lines(&jsonl_path).unwrap_or(0);
        let dependency_count = count_dependencies(&jsonl_path).unwrap_or(0);
        let content_hash = hash_beads_directory(&beads_dir)?;

        // Get git commit from source repository (if .git exists)
        let source_commit = get_git_commit(&dataset.source_path());

        let metadata = DatasetMetadata {
            name: dataset.name().to_string(),
            source_path: dataset.source_path(),
            issue_count,
            jsonl_size_bytes,
            db_size_bytes,
            dependency_count,
            content_hash,
            copied_at: Some(SystemTime::now()),
            copy_duration: Some(copy_duration),
            source_commit,
            is_override: false,
            override_reason: None,
        };

        Ok(Self {
            temp_dir,
            root,
            beads_dir,
            metadata,
            source_dataset: dataset,
        })
    }

    /// Create an empty isolated workspace (for init tests).
    pub fn empty() -> std::io::Result<Self> {
        let temp_dir = TempDir::new()?;
        let root = temp_dir.path().to_path_buf();
        let beads_dir = root.join(".beads");

        // Create minimal git scaffold
        fs::create_dir_all(root.join(".git"))?;
        fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n")?;

        let metadata = DatasetMetadata {
            name: "empty".to_string(),
            source_path: PathBuf::new(),
            issue_count: 0,
            jsonl_size_bytes: 0,
            db_size_bytes: 0,
            dependency_count: 0,
            content_hash: "empty".to_string(),
            copied_at: Some(SystemTime::now()),
            copy_duration: Some(Duration::ZERO),
            source_commit: None,
            is_override: false,
            override_reason: None,
        };

        Ok(Self {
            temp_dir,
            root,
            beads_dir,
            metadata,
            source_dataset: KnownDataset::BeadsRust, // Placeholder
        })
    }

    /// Get the path to the workspace root (for cwd).
    pub fn workspace_root(&self) -> &Path {
        &self.root
    }

    /// Upgrade the isolated copy through br's reviewed schema-migration
    /// workflow. The source dataset remains untouched.
    pub fn migrate_to_current_schema(&self) -> std::io::Result<()> {
        migrate_workspace_to_current_schema(&self.root)
    }

    /// Get path to log directory (creates if needed).
    pub fn log_dir(&self) -> PathBuf {
        let dir = self.root.join("test-artifacts");
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// Write summary.json with dataset metadata.
    pub fn write_summary(&self) -> std::io::Result<PathBuf> {
        let summary_path = self.log_dir().join("summary.json");
        let summary = serde_json::json!({
            "dataset": self.metadata.to_json(),
            "workspace_root": self.root.display().to_string(),
            "beads_dir": self.beads_dir.display().to_string(),
        });
        fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)?;
        Ok(summary_path)
    }
}

/// Upgrade a copied fixture through the public plan/apply workflow.
pub fn migrate_workspace_to_current_schema(root: &Path) -> std::io::Result<()> {
    let binary = assert_cmd::cargo::cargo_bin!("br");
    let plan = std::process::Command::new(binary)
        .args(["doctor", "migrate-schema", "plan", "--json"])
        .current_dir(root)
        .env("NO_COLOR", "1")
        .output()?;
    if !plan.status.success() {
        let plan_stdout = String::from_utf8_lossy(&plan.stdout);
        let plan_stderr = String::from_utf8_lossy(&plan.stderr);
        // Sources below the reviewed-migration floor (schemas before 13) have
        // no reviewed plan/apply pair, so `plan` refuses outright instead of
        // reporting an ineligible plan. Surface that refusal as
        // `ErrorKind::Unsupported` so callers can fall back to the JSONL-only
        // rebuild contract instead of treating it as a harness failure.
        if plan_stdout.contains("unsupported source version")
            || plan_stderr.contains("unsupported source version")
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!(
                    "schema migration plan refused pre-floor source: stdout={plan_stdout} stderr={plan_stderr}"
                ),
            ));
        }
        return Err(std::io::Error::other(format!(
            "schema migration plan failed: stdout={plan_stdout} stderr={plan_stderr}"
        )));
    }
    let plan_json: serde_json::Value = serde_json::from_slice(&plan.stdout).map_err(|error| {
        std::io::Error::other(format!(
            "schema migration plan emitted invalid JSON ({error}): {}",
            String::from_utf8_lossy(&plan.stdout)
        ))
    })?;
    if plan_json
        .get("eligible")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
    {
        return Ok(());
    }
    let token = plan_json
        .get("plan_token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| std::io::Error::other("eligible migration plan omitted plan_token"))?;
    let apply = std::process::Command::new(binary)
        .args([
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            token,
            "--json",
        ])
        .current_dir(root)
        .env("NO_COLOR", "1")
        .output()?;
    if !apply.status.success() {
        return Err(std::io::Error::other(format!(
            "schema migration apply failed: stdout={} stderr={}",
            String::from_utf8_lossy(&apply.stdout),
            String::from_utf8_lossy(&apply.stderr)
        )));
    }
    Ok(())
}

/// Copy a directory recursively, respecting the sync allowlist.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);

        // Skip socket files (like bd.sock)
        let name = file_name.to_string_lossy();
        if name.ends_with(".sock") {
            continue;
        }

        // Skip SQLite sidecars (will be regenerated)
        if name.ends_with("-wal")
            || name.ends_with("-wal-cert")
            || name.ends_with("-wal-cert-head")
            || name.ends_with("-shm")
            || name.ends_with("-journal")
            || name.ends_with("-fsqlite-ns-gate")
            || name.ends_with("-fsqlite-ns-use")
        {
            continue;
        }

        // Skip sync lock
        if name == ".sync.lock" {
            continue;
        }

        if file_type.is_dir() {
            // Skip history subdirectory (can be large, recreated as needed)
            if name == "history" {
                continue;
            }
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

fn copy_fixture_workspace_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_fixture_workspace_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

fn copy_workspace_failure_fixture_root(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;

    let visible_payload = src.join("beads");
    let hidden_payload = src.join(".beads");
    let payload = if visible_payload.exists() {
        Some(visible_payload)
    } else if hidden_payload.exists() {
        Some(hidden_payload)
    } else {
        None
    };

    if let Some(payload) = payload {
        copy_fixture_workspace_recursive(&payload, &dst.join(".beads"))?;
    }

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if matches!(name.as_ref(), "fixture.json" | "beads" | ".beads") {
            continue;
        }

        let src_path = entry.path();
        let dst_path = dst.join(file_name);
        if file_type.is_dir() {
            copy_fixture_workspace_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

/// Count lines in a JSONL file (approximation of issue count).
fn count_jsonl_lines(path: &Path) -> std::io::Result<usize> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    Ok(reader.lines().count())
}

/// Count dependencies by parsing JSONL (looks for "dependencies" arrays).
fn count_dependencies(path: &Path) -> std::io::Result<usize> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut count = 0;

    for line in reader.lines() {
        let line = line?;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
            && let Some(deps) = value.get("dependencies").and_then(|d| d.as_array())
        {
            count += deps.len();
        }
    }

    Ok(count)
}

/// Hash the contents of a .beads directory for integrity verification.
fn hash_beads_directory(beads_dir: &Path) -> std::io::Result<String> {
    let mut hasher = Sha256::new();

    // Hash key files in deterministic order
    let files_to_hash = ["issues.jsonl", "config.yaml"];

    for filename in &files_to_hash {
        let path = beads_dir.join(filename);
        if path.exists() {
            let mut file = File::open(&path)?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)?;
            hasher.update(&buffer);
        }
    }

    Ok(beads_rust::util::hex_encode(&hasher.finalize())[..16].to_string())
}

fn normalize_source_commit(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.len() >= 40 && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Some(trimmed[..7].to_string());
    }

    Some(trimmed.to_string())
}

fn source_commit_override_with(get_env: impl Fn(&str) -> Option<String>) -> Option<String> {
    SOURCE_COMMIT_OVERRIDE_ENVS
        .iter()
        .find_map(|name| get_env(name).and_then(|value| normalize_source_commit(&value)))
}

fn source_commit_override() -> Option<String> {
    source_commit_override_with(|name| std::env::var(name).ok())
}

fn current_repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn compile_time_git_commit(repo_path: &Path) -> Option<String> {
    if !repo_path.starts_with(current_repo_root()) {
        return None;
    }

    option_env!("VERGEN_GIT_SHA").and_then(normalize_source_commit)
}

/// Get git commit hash from a repository.
///
/// This deliberately does not require `repo_path/.git` to exist because:
/// - Git can discover parent repositories from a subdirectory.
/// - Worktrees and offloaded environments may provide Git context without a
///   literal `.git` entry at the dataset root.
/// - RCH/offloaded runs can inject a stable override when Git metadata is not
///   available in the synced checkout.
fn get_git_commit(repo_path: &Path) -> Option<String> {
    if let Some(override_commit) = source_commit_override() {
        return Some(override_commit);
    }

    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| normalize_source_commit(&String::from_utf8_lossy(&output.stdout)))
        .or_else(|| compile_time_git_commit(repo_path))
}

// =============================================================================
// Dataset Override Support (beads_rust-b4nj)
// =============================================================================

/// Configuration for dataset override.
///
/// Allows tests to use custom `.beads` directories instead of known datasets.
#[derive(Debug, Clone)]
pub struct DatasetOverride {
    /// Custom path to use instead of known dataset
    pub path: PathBuf,
    /// Reason for the override (logged for traceability)
    pub reason: String,
    /// Optional name override (defaults to directory name)
    pub name: Option<String>,
}

impl DatasetOverride {
    /// Create a new dataset override.
    pub fn new(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            reason: reason.into(),
            name: None,
        }
    }

    /// Create with a custom name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// Create an isolated dataset from a custom path (override).
///
/// This allows tests to use arbitrary `.beads` directories instead of
/// the known datasets. The override is logged for traceability.
pub fn isolated_from_override(
    override_config: &DatasetOverride,
) -> std::io::Result<IsolatedDataset> {
    let source_path = &override_config.path;
    let source_beads_dir = source_path.join(".beads");

    if !source_beads_dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "Override dataset not found at {}",
                source_beads_dir.display()
            ),
        ));
    }

    let start = Instant::now();
    let temp_dir = TempDir::new()?;
    let root = temp_dir.path().to_path_buf();
    let beads_dir = root.join(".beads");

    // Copy .beads directory
    copy_dir_recursive(&source_beads_dir, &beads_dir)?;

    // Create minimal repo scaffold
    fs::create_dir_all(root.join(".git"))?;
    fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n")?;

    let copy_duration = start.elapsed();

    // Scan copied dataset for metadata
    let jsonl_path = beads_dir.join("issues.jsonl");
    let db_path = beads_dir.join("beads.db");

    let jsonl_size_bytes = fs::metadata(&jsonl_path).map_or(0, |m| m.len());
    let db_size_bytes = fs::metadata(&db_path).map_or(0, |m| m.len());
    let issue_count = count_jsonl_lines(&jsonl_path).unwrap_or(0);
    let dependency_count = count_dependencies(&jsonl_path).unwrap_or(0);
    let content_hash = hash_beads_directory(&beads_dir)?;

    // Get git commit from source repository (if .git exists)
    let source_commit = get_git_commit(source_path);

    // Derive name from directory or use override
    let name = override_config
        .name
        .clone()
        .or_else(|| {
            source_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "override".to_string());

    let metadata = DatasetMetadata {
        name,
        source_path: source_path.clone(),
        issue_count,
        jsonl_size_bytes,
        db_size_bytes,
        dependency_count,
        content_hash,
        copied_at: Some(SystemTime::now()),
        copy_duration: Some(copy_duration),
        source_commit,
        is_override: true,
        override_reason: Some(override_config.reason.clone()),
    };

    // Log the override for traceability
    eprintln!(
        "[dataset_registry] Using override dataset: {} (reason: {})",
        source_path.display(),
        override_config.reason
    );

    Ok(IsolatedDataset {
        temp_dir,
        root,
        beads_dir,
        metadata,
        source_dataset: KnownDataset::BeadsRust, // Placeholder for overrides
    })
}

// =============================================================================
// Dataset Integrity Guard (beads_rust-b4nj)
// =============================================================================

/// Integrity verification result.
#[derive(Debug, Clone)]
pub struct IntegrityCheckResult {
    /// Whether the check passed
    pub passed: bool,
    /// Original hash captured at guard creation
    pub original_hash: String,
    /// Current hash at verification time
    pub current_hash: String,
    /// Human-readable message describing the result
    pub message: String,
}

impl IntegrityCheckResult {
    /// Assert that the integrity check passed.
    pub fn assert_ok(&self) {
        assert!(self.passed, "{}", self.message);
    }

    /// Convert to JSON for logging.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "passed": self.passed,
            "original_hash": self.original_hash,
            "current_hash": self.current_hash,
            "message": self.message,
        })
    }
}

/// Guard that verifies source dataset integrity before and after test operations.
///
/// Use this to ensure that tests don't accidentally mutate source datasets.
/// The guard captures the hash at creation and can verify it hasn't changed.
///
/// # Example
///
/// ```ignore
/// let mut guard = DatasetIntegrityGuard::new(KnownDataset::BeadsRust)?;
/// guard.verify_before().assert_ok();
///
/// // ... run tests ...
///
/// guard.verify_after().assert_ok();
/// ```
pub struct DatasetIntegrityGuard {
    dataset_name: String,
    source_path: PathBuf,
    original_hash: String,
    verified_before: bool,
    verified_after: bool,
}

impl DatasetIntegrityGuard {
    /// Create a new integrity guard for a known dataset.
    ///
    /// Captures the current hash of the source dataset.
    pub fn new(dataset: KnownDataset) -> std::io::Result<Self> {
        let beads_dir = dataset.beads_dir();
        let original_hash = hash_beads_directory(&beads_dir)?;

        Ok(Self {
            dataset_name: dataset.name().to_string(),
            source_path: dataset.source_path(),
            original_hash,
            verified_before: false,
            verified_after: false,
        })
    }

    /// Create a guard from a custom path (for overrides).
    pub fn from_path(path: impl Into<PathBuf>, name: impl Into<String>) -> std::io::Result<Self> {
        let source_path: PathBuf = path.into();
        let beads_dir = source_path.join(".beads");
        let original_hash = hash_beads_directory(&beads_dir)?;

        Ok(Self {
            dataset_name: name.into(),
            source_path,
            original_hash,
            verified_before: false,
            verified_after: false,
        })
    }

    /// Verify source integrity before copy.
    ///
    /// Call this before copying the dataset to verify it starts in a known state.
    pub fn verify_before(&mut self) -> IntegrityCheckResult {
        self.verified_before = true;
        self.verify_current("before")
    }

    /// Verify source integrity after test operations.
    ///
    /// Call this after test operations to ensure the source wasn't mutated.
    pub fn verify_after(&mut self) -> IntegrityCheckResult {
        self.verified_after = true;
        self.verify_current("after")
    }

    /// Verify current state matches original.
    fn verify_current(&self, phase: &str) -> IntegrityCheckResult {
        let beads_dir = self.source_path.join(".beads");
        let current_hash = hash_beads_directory(&beads_dir).unwrap_or_else(|_| "ERROR".to_string());

        let passed = current_hash == self.original_hash;
        let message = if passed {
            format!(
                "[{}] Source dataset '{}' integrity verified (hash: {})",
                phase,
                self.dataset_name,
                &self.original_hash[..self.original_hash.len().min(8)]
            )
        } else {
            format!(
                "[{}] SOURCE DATASET '{}' WAS MUTATED! Original: {}, Current: {}",
                phase, self.dataset_name, self.original_hash, current_hash
            )
        };

        IntegrityCheckResult {
            passed,
            original_hash: self.original_hash.clone(),
            current_hash,
            message,
        }
    }

    /// Get the original hash.
    pub fn original_hash(&self) -> &str {
        &self.original_hash
    }

    /// Get the dataset name.
    pub fn dataset_name(&self) -> &str {
        &self.dataset_name
    }

    /// Check if both before and after verifications were performed.
    pub const fn fully_verified(&self) -> bool {
        self.verified_before && self.verified_after
    }

    /// Convert to JSON for logging in summary.json.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "dataset_name": self.dataset_name,
            "source_path": self.source_path.display().to_string(),
            "original_hash": self.original_hash,
            "verified_before": self.verified_before,
            "verified_after": self.verified_after,
        })
    }
}

// =============================================================================
// Provenance Logging (beads_rust-b4nj)
// =============================================================================

/// Full provenance information for a test run.
///
/// This captures everything needed to reproduce the test environment.
#[derive(Debug, Clone)]
pub struct DatasetProvenance {
    /// Dataset metadata (name, hashes, counts)
    pub metadata: DatasetMetadata,
    /// Integrity guard results (if used)
    pub integrity_before: Option<IntegrityCheckResult>,
    pub integrity_after: Option<IntegrityCheckResult>,
    /// Test start timestamp
    pub started_at: SystemTime,
    /// Additional context (test name, scenario, etc.)
    pub context: HashMap<String, String>,
}

impl DatasetProvenance {
    /// Create provenance from dataset metadata.
    pub fn from_metadata(metadata: DatasetMetadata) -> Self {
        Self {
            metadata,
            integrity_before: None,
            integrity_after: None,
            started_at: SystemTime::now(),
            context: HashMap::new(),
        }
    }

    /// Create provenance from an isolated dataset.
    pub fn from_isolated(isolated: &IsolatedDataset) -> Self {
        Self::from_metadata(isolated.metadata.clone())
    }

    /// Add integrity guard results (before check).
    pub fn with_integrity_before(mut self, result: IntegrityCheckResult) -> Self {
        self.integrity_before = Some(result);
        self
    }

    /// Add integrity guard results (after check).
    pub fn with_integrity_after(mut self, result: IntegrityCheckResult) -> Self {
        self.integrity_after = Some(result);
        self
    }

    /// Add context value.
    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }

    /// Serialize to JSON for summary.json.
    pub fn to_json(&self) -> serde_json::Value {
        let mut json = serde_json::json!({
            "dataset": self.metadata.to_json(),
            "started_at": format!("{:?}", self.started_at),
        });

        if let Some(ref before) = self.integrity_before {
            json["integrity_before"] = before.to_json();
        }

        if let Some(ref after) = self.integrity_after {
            json["integrity_after"] = after.to_json();
        }

        if !self.context.is_empty() {
            json["context"] = serde_json::json!(self.context);
        }

        json
    }

    /// Write provenance to a summary.json file.
    pub fn write_to_file(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(&self.to_json()).map_err(std::io::Error::other)?;
        fs::write(path, json)
    }
}

/// Helper to run a test with full integrity verification.
///
/// This is a convenience function that:
/// 1. Creates an integrity guard
/// 2. Verifies before
/// 3. Creates an isolated dataset
/// 4. Runs the test function
/// 5. Verifies after
/// 6. Returns provenance with results
///
/// # Example
///
/// ```ignore
/// let provenance = run_with_integrity(KnownDataset::BeadsRust, |isolated| {
///     // ... run test commands on isolated.workspace_root() ...
///     Ok(())
/// })?;
/// provenance.integrity_after.unwrap().assert_ok();
/// ```
pub fn run_with_integrity<F, T>(
    dataset: KnownDataset,
    test_fn: F,
) -> std::io::Result<(T, DatasetProvenance)>
where
    F: FnOnce(&IsolatedDataset) -> std::io::Result<T>,
{
    // Create integrity guard and verify before
    let mut guard = DatasetIntegrityGuard::new(dataset)?;
    let before_result = guard.verify_before();

    // Fail fast if source is already corrupted
    if !before_result.passed {
        return Err(std::io::Error::other(before_result.message));
    }

    // Create isolated dataset
    let isolated = IsolatedDataset::from_dataset(dataset)?;

    // Run the test function
    let result = test_fn(&isolated)?;

    // Verify after
    let after_result = guard.verify_after();

    // Build provenance
    let provenance = DatasetProvenance::from_isolated(&isolated)
        .with_integrity_before(before_result)
        .with_integrity_after(after_result);

    Ok((result, provenance))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated_beads_rust_or_skip(test_name: &str) -> Option<IsolatedDataset> {
        let registry = DatasetRegistry::new();
        if !registry.is_available(KnownDataset::BeadsRust) {
            eprintln!("Skipping {test_name}: beads_rust dataset not available (no beads.db in CI)");
            return None;
        }

        match IsolatedDataset::from_dataset(KnownDataset::BeadsRust) {
            Ok(isolated) => Some(isolated),
            Err(error) => {
                eprintln!("Skipping {test_name}: failed to copy beads_rust dataset: {error}");
                None
            }
        }
    }

    #[test]
    fn test_registry_creation() {
        let registry = DatasetRegistry::new();
        // beads_rust may not be available in CI (no beads.db)
        // Just verify the registry can be created
        let _ = registry.is_available(KnownDataset::BeadsRust);
    }

    #[test]
    fn test_isolated_dataset_copy() {
        let Some(isolated) = isolated_beads_rust_or_skip("test_isolated_dataset_copy") else {
            return;
        };

        // Verify the copy was created
        assert!(isolated.beads_dir.exists());
        assert!(isolated.beads_dir.join("beads.db").exists());

        // Verify metadata was captured
        assert_eq!(isolated.metadata.name, "beads_rust");
        assert!(isolated.metadata.issue_count > 0);
        assert!(isolated.metadata.copy_duration.is_some());
    }

    #[test]
    fn test_empty_workspace() {
        let isolated = IsolatedDataset::empty().expect("should create empty workspace");

        // Verify workspace structure
        assert!(isolated.root.exists());
        assert!(isolated.root.join(".git").exists());

        // Beads dir should not exist yet (init will create it)
        assert!(!isolated.beads_dir.exists());
    }

    /// Helper to check if `beads_rust` dataset is available (has `beads.db`)
    fn beads_rust_available() -> bool {
        DatasetRegistry::new().is_available(KnownDataset::BeadsRust)
    }

    #[test]
    fn test_source_integrity_check() {
        if !beads_rust_available() {
            eprintln!("Skipping test_source_integrity_check: beads_rust dataset not available");
            return;
        }

        let registry = DatasetRegistry::new();

        // This should pass (source unchanged during test)
        let result = registry.verify_source_integrity(KnownDataset::BeadsRust);
        assert!(result.is_ok(), "Source integrity check failed: {result:?}");
    }

    // =========================================================================
    // DatasetIntegrityGuard tests (beads_rust-b4nj)
    // =========================================================================

    #[test]
    fn test_integrity_guard_creation() {
        if !beads_rust_available() {
            eprintln!("Skipping test_integrity_guard_creation: beads_rust dataset not available");
            return;
        }

        let guard =
            DatasetIntegrityGuard::new(KnownDataset::BeadsRust).expect("should create guard");

        assert_eq!(guard.dataset_name(), "beads_rust");
        assert!(!guard.original_hash().is_empty());
        assert!(!guard.fully_verified()); // Not yet verified
    }

    #[test]
    fn test_integrity_guard_verify_before() {
        if !beads_rust_available() {
            eprintln!(
                "Skipping test_integrity_guard_verify_before: beads_rust dataset not available"
            );
            return;
        }

        let mut guard =
            DatasetIntegrityGuard::new(KnownDataset::BeadsRust).expect("should create guard");

        let result = guard.verify_before();
        assert!(result.passed, "Before check failed: {}", result.message);
        assert_eq!(result.original_hash, result.current_hash);
    }

    #[test]
    fn test_integrity_guard_verify_after() {
        if !beads_rust_available() {
            eprintln!(
                "Skipping test_integrity_guard_verify_after: beads_rust dataset not available"
            );
            return;
        }

        let mut guard =
            DatasetIntegrityGuard::new(KnownDataset::BeadsRust).expect("should create guard");

        // Verify both before and after
        let before = guard.verify_before();
        assert!(before.passed);

        // Source shouldn't change during test
        let after = guard.verify_after();
        assert!(after.passed, "After check failed: {}", after.message);

        assert!(guard.fully_verified());
    }

    #[test]
    fn test_integrity_guard_to_json() {
        if !beads_rust_available() {
            eprintln!("Skipping test_integrity_guard_to_json: beads_rust dataset not available");
            return;
        }

        let mut guard =
            DatasetIntegrityGuard::new(KnownDataset::BeadsRust).expect("should create guard");

        guard.verify_before();
        guard.verify_after();

        let json = guard.to_json();
        assert_eq!(json["dataset_name"], "beads_rust");
        assert_eq!(json["verified_before"], true);
        assert_eq!(json["verified_after"], true);
        assert!(json["original_hash"].is_string());
    }

    #[test]
    fn test_integrity_check_result_to_json() {
        let result = IntegrityCheckResult {
            passed: true,
            original_hash: "abc123".to_string(),
            current_hash: "abc123".to_string(),
            message: "Test passed".to_string(),
        };

        let json = result.to_json();
        assert_eq!(json["passed"], true);
        assert_eq!(json["original_hash"], "abc123");
        assert_eq!(json["message"], "Test passed");
    }

    // =========================================================================
    // DatasetOverride tests (beads_rust-b4nj)
    // =========================================================================

    #[test]
    fn test_dataset_override_creation() {
        let override_cfg = DatasetOverride::new("/some/path", "testing override feature");

        assert_eq!(override_cfg.path, PathBuf::from("/some/path"));
        assert_eq!(override_cfg.reason, "testing override feature");
        assert!(override_cfg.name.is_none());
    }

    #[test]
    fn test_dataset_override_with_name() {
        let override_cfg = DatasetOverride::new("/some/path", "test").with_name("custom_name");

        assert_eq!(override_cfg.name, Some("custom_name".to_string()));
    }

    #[test]
    fn test_isolated_from_override() {
        if !beads_rust_available() {
            eprintln!("Skipping test_isolated_from_override: beads_rust dataset not available");
            return;
        }

        // Use beads_rust as the override source (we know it exists)
        let override_cfg = DatasetOverride::new(
            KnownDataset::BeadsRust.source_path(),
            "testing override with beads_rust",
        )
        .with_name("override_test");

        let isolated =
            isolated_from_override(&override_cfg).expect("should create isolated from override");

        // Verify metadata reflects override
        assert_eq!(isolated.metadata.name, "override_test");
        assert!(isolated.metadata.is_override);
        assert_eq!(
            isolated.metadata.override_reason,
            Some("testing override with beads_rust".to_string())
        );
        assert!(isolated.metadata.issue_count > 0);
    }

    #[test]
    fn test_isolated_from_override_missing_path() {
        let override_cfg = DatasetOverride::new("/nonexistent/path", "test");

        let result = isolated_from_override(&override_cfg);
        assert!(result.is_err());
    }

    #[test]
    fn test_workspace_failure_fixture_catalog_is_loadable() {
        let fixtures = list_workspace_failure_fixtures().expect("fixture catalog");
        let names: Vec<&str> = fixtures
            .iter()
            .map(|fixture| fixture.metadata.name.as_str())
            .collect();

        assert_eq!(
            names,
            vec![
                "corrupt_db_text",
                "db_jsonl_disagreement",
                "duplicate_config_rows",
                "interrupted_rebuild_leftovers",
                "journal_sidecar_leftover",
                "jsonl_conflict_markers",
                "metadata_custom_paths",
                "orphan_shm_sidecar",
                "orphaned_lock_file",
                "sidecar_wal_without_shm",
            ]
        );
        for fixture in &fixtures {
            assert!(
                fixture.metadata.outcome_for("startup/open").is_some(),
                "{} missing startup/open expectation",
                fixture.metadata.name
            );
            assert!(
                !fixture.metadata.expected_command_outcomes.is_empty(),
                "{} missing command expectations",
                fixture.metadata.name
            );
        }
    }

    #[test]
    fn test_isolated_workspace_failure_fixture_preserves_sidecars_and_recovery_debris() {
        let wal_fixture =
            isolated_workspace_failure_fixture("sidecar_wal_without_shm").expect("sidecar fixture");
        assert!(
            wal_fixture.beads_dir.join("beads.db-wal").exists(),
            "sidecar WAL should be preserved in copied fixture"
        );

        let rebuild_fixture = isolated_workspace_failure_fixture("interrupted_rebuild_leftovers")
            .expect("interrupted rebuild fixture");
        assert!(
            rebuild_fixture
                .beads_dir
                .join("beads.db.bad_20260312T000000Z")
                .exists(),
            "backup database should be preserved in copied fixture"
        );
        assert!(
            rebuild_fixture
                .beads_dir
                .join(".br_recovery")
                .join("beads.db.20260312T000000Z.rebuild-failed")
                .exists(),
            "recovery debris should be preserved in copied fixture"
        );
    }

    #[test]
    fn test_isolated_workspace_failure_fixture_preserves_custom_metadata_targets() {
        let fixture = isolated_workspace_failure_fixture("metadata_custom_paths")
            .expect("metadata override fixture");

        assert!(
            fixture.beads_dir.join("custom.db").exists(),
            "custom database path should be preserved"
        );
        assert!(
            fixture.beads_dir.join("custom.jsonl").exists(),
            "custom jsonl path should be preserved"
        );
    }

    // =========================================================================
    // DatasetProvenance tests (beads_rust-b4nj)
    // =========================================================================

    #[test]
    fn test_provenance_from_metadata() {
        let metadata = DatasetMetadata {
            name: "test".to_string(),
            source_path: PathBuf::from("/test"),
            issue_count: 10,
            jsonl_size_bytes: 1000,
            db_size_bytes: 2000,
            dependency_count: 5,
            content_hash: "hash123".to_string(),
            copied_at: Some(SystemTime::now()),
            copy_duration: Some(Duration::from_millis(100)),
            source_commit: Some("abc1234".to_string()),
            is_override: false,
            override_reason: None,
        };

        let provenance = DatasetProvenance::from_metadata(metadata);
        assert_eq!(provenance.metadata.name, "test");
        assert!(provenance.integrity_before.is_none());
        assert!(provenance.integrity_after.is_none());
    }

    #[test]
    fn test_provenance_with_context() {
        let metadata = DatasetMetadata {
            name: "test".to_string(),
            source_path: PathBuf::new(),
            issue_count: 0,
            jsonl_size_bytes: 0,
            db_size_bytes: 0,
            dependency_count: 0,
            content_hash: "hash".to_string(),
            copied_at: None,
            copy_duration: None,
            source_commit: None,
            is_override: false,
            override_reason: None,
        };

        let provenance = DatasetProvenance::from_metadata(metadata)
            .with_context("test_name", "my_test")
            .with_context("scenario", "basic");

        assert_eq!(
            provenance.context.get("test_name"),
            Some(&"my_test".to_string())
        );
        assert_eq!(
            provenance.context.get("scenario"),
            Some(&"basic".to_string())
        );
    }

    #[test]
    fn test_provenance_to_json() {
        let metadata = DatasetMetadata {
            name: "test".to_string(),
            source_path: PathBuf::from("/test"),
            issue_count: 10,
            jsonl_size_bytes: 1000,
            db_size_bytes: 2000,
            dependency_count: 5,
            content_hash: "hash123".to_string(),
            copied_at: None,
            copy_duration: None,
            source_commit: Some("abc1234".to_string()),
            is_override: false,
            override_reason: None,
        };

        let before_result = IntegrityCheckResult {
            passed: true,
            original_hash: "hash123".to_string(),
            current_hash: "hash123".to_string(),
            message: "OK".to_string(),
        };

        let provenance = DatasetProvenance::from_metadata(metadata)
            .with_integrity_before(before_result)
            .with_context("test", "value");

        let json = provenance.to_json();

        assert!(json["dataset"].is_object());
        assert!(json["started_at"].is_string());
        assert!(json["integrity_before"]["passed"].as_bool().unwrap());
        assert_eq!(json["context"]["test"], "value");
    }

    // =========================================================================
    // run_with_integrity tests (beads_rust-b4nj)
    // =========================================================================

    #[test]
    fn test_run_with_integrity() {
        let registry = DatasetRegistry::new();
        if !registry.is_available(KnownDataset::BeadsRust) {
            eprintln!("Skipping test_run_with_integrity: beads_rust dataset not available");
            return;
        }

        let (result, provenance) = run_with_integrity(KnownDataset::BeadsRust, |isolated| {
            // Verify we have a valid isolated dataset
            assert!(isolated.beads_dir.exists());
            assert!(isolated.metadata.issue_count > 0);
            Ok(42) // Return a value to verify it's passed through
        })
        .expect("should run with integrity");

        // Verify the result was passed through
        assert_eq!(result, 42);

        // Verify integrity checks were performed
        assert!(provenance.integrity_before.is_some());
        assert!(provenance.integrity_after.is_some());
        provenance.integrity_before.as_ref().unwrap().assert_ok();
        provenance.integrity_after.as_ref().unwrap().assert_ok();
    }

    // =========================================================================
    // Metadata enhancement tests (beads_rust-b4nj)
    // =========================================================================

    #[test]
    fn test_metadata_includes_source_commit() {
        let Some(isolated) = isolated_beads_rust_or_skip("test_metadata_includes_source_commit")
        else {
            return;
        };

        let expected_commit = get_git_commit(&KnownDataset::BeadsRust.source_path());

        assert_eq!(
            isolated.metadata.source_commit, expected_commit,
            "source_commit should match git availability for the source dataset"
        );
    }

    #[test]
    fn test_metadata_to_json_includes_new_fields() {
        let Some(isolated) =
            isolated_beads_rust_or_skip("test_metadata_to_json_includes_new_fields")
        else {
            return;
        };

        let json = isolated.metadata.to_json();

        // Verify new fields are present
        assert!(json.get("source_commit").is_some());
        assert!(json.get("is_override").is_some());
        assert!(json.get("override_reason").is_some());

        // Verify values
        assert_eq!(json["is_override"], false);
        assert!(json["override_reason"].is_null());
    }

    #[test]
    fn test_empty_workspace_has_no_source_commit() {
        let isolated = IsolatedDataset::empty().expect("should create empty workspace");

        assert!(
            isolated.metadata.source_commit.is_none(),
            "empty workspace should have no source_commit"
        );
    }

    #[test]
    fn test_get_git_commit_discovers_parent_repo_from_subdir() {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_subdir = repo_root.join("src");

        assert!(
            !repo_subdir.join(".git").exists(),
            "test requires a normal repo subdirectory without its own .git entry"
        );

        assert_eq!(
            get_git_commit(&repo_subdir),
            get_git_commit(&repo_root),
            "git commit detection should work from subdirectories via parent repo discovery"
        );
    }

    #[test]
    fn test_source_commit_override_trims_and_ignores_empty_values() {
        assert_eq!(
            source_commit_override_with(|_| Some(" abc1234 \n".to_string())),
            Some("abc1234".to_string())
        );
        assert_eq!(
            source_commit_override_with(|_| Some("   ".to_string())),
            None
        );
        assert_eq!(source_commit_override_with(|_| None), None);
    }

    #[test]
    fn test_source_commit_override_uses_rch_fallback_envs() {
        assert_eq!(
            source_commit_override_with(|name| {
                (name == "RCH_GIT_SHA")
                    .then(|| "0123456789abcdef0123456789abcdef01234567".to_string())
            }),
            Some("0123456".to_string())
        );
    }

    #[test]
    fn test_source_commit_override_prefers_explicit_dataset_env() {
        assert_eq!(
            source_commit_override_with(|name| match name {
                SOURCE_COMMIT_OVERRIDE_ENV => Some("primary123".to_string()),
                "RCH_GIT_SHA" => Some("fallback456".to_string()),
                _ => None,
            }),
            Some("primary123".to_string())
        );
    }

    #[test]
    fn test_normalize_source_commit_trims_empty_and_shortens_full_sha() {
        assert_eq!(normalize_source_commit("   "), None);
        assert_eq!(
            normalize_source_commit("abcdef0123456789abcdef0123456789abcdef01"),
            Some("abcdef0".to_string())
        );
        assert_eq!(
            normalize_source_commit("not-a-hex-build-id"),
            Some("not-a-hex-build-id".to_string())
        );
    }
}
