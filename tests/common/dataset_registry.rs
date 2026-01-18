//! Dataset registry for E2E, conformance, and benchmark tests.
//!
//! Provides access to real `.beads` directories as fixtures, with safe copy
//! to isolated temp workspaces. Source datasets are NEVER mutated.

#![allow(dead_code)]

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
    /// Override reason/description (if is_override is true)
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

/// Known datasets for testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KnownDataset {
    BeadsRust,
    BeadsViewer,
    CodingAgentSessionSearch,
    BrennerBot,
}

impl KnownDataset {
    pub fn name(&self) -> &'static str {
        match self {
            Self::BeadsRust => "beads_rust",
            Self::BeadsViewer => "beads_viewer",
            Self::CodingAgentSessionSearch => "coding_agent_session_search",
            Self::BrennerBot => "brenner_bot",
        }
    }

    pub fn source_path(&self) -> PathBuf {
        PathBuf::from(match self {
            Self::BeadsRust => "/data/projects/beads_rust",
            Self::BeadsViewer => "/data/projects/beads_viewer",
            Self::CodingAgentSessionSearch => "/data/projects/coding_agent_session_search",
            Self::BrennerBot => "/data/projects/brenner_bot",
        })
    }

    pub fn beads_dir(&self) -> PathBuf {
        self.source_path().join(".beads")
    }

    pub fn all() -> &'static [KnownDataset] {
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

impl DatasetRegistry {
    /// Create a new registry, scanning available datasets.
    pub fn new() -> Self {
        let mut registry = Self {
            datasets: HashMap::new(),
            source_hashes: HashMap::new(),
        };

        for dataset in KnownDataset::all() {
            if let Ok(metadata) = registry.scan_dataset(*dataset) {
                registry.source_hashes.insert(
                    dataset.name().to_string(),
                    metadata.content_hash.clone(),
                );
                registry.datasets.insert(dataset.name().to_string(), metadata);
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
    fn scan_dataset(&self, dataset: KnownDataset) -> std::io::Result<DatasetMetadata> {
        let beads_dir = dataset.beads_dir();
        if !beads_dir.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Dataset {} not found at {}", dataset.name(), beads_dir.display()),
            ));
        }

        let jsonl_path = beads_dir.join("issues.jsonl");
        let db_path = beads_dir.join("beads.db");

        let jsonl_size_bytes = fs::metadata(&jsonl_path).map(|m| m.len()).unwrap_or(0);
        let db_size_bytes = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

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

        let jsonl_size_bytes = fs::metadata(&jsonl_path).map(|m| m.len()).unwrap_or(0);
        let db_size_bytes = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
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

        // Skip WAL/SHM files (will be regenerated)
        if name.ends_with("-wal") || name.ends_with("-shm") {
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
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(deps) = value.get("dependencies").and_then(|d| d.as_array()) {
                count += deps.len();
            }
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

    Ok(format!("{:x}", hasher.finalize())[..16].to_string())
}

/// Get git commit hash from a repository (if .git exists and git is available).
fn get_git_commit(repo_path: &Path) -> Option<String> {
    let git_dir = repo_path.join(".git");
    if !git_dir.exists() {
        return None;
    }

    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
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
pub fn isolated_from_override(override_config: &DatasetOverride) -> std::io::Result<IsolatedDataset> {
    let source_path = &override_config.path;
    let source_beads_dir = source_path.join(".beads");

    if !source_beads_dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Override dataset not found at {}", source_beads_dir.display()),
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

    let jsonl_size_bytes = fs::metadata(&jsonl_path).map(|m| m.len()).unwrap_or(0);
    let db_size_bytes = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
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
        let current_hash = hash_beads_directory(&beads_dir)
            .unwrap_or_else(|_| "ERROR".to_string());

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
                phase,
                self.dataset_name,
                self.original_hash,
                current_hash
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
    pub fn fully_verified(&self) -> bool {
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
        let json = serde_json::to_string_pretty(&self.to_json())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
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
pub fn run_with_integrity<F, T>(dataset: KnownDataset, test_fn: F) -> std::io::Result<(T, DatasetProvenance)>
where
    F: FnOnce(&IsolatedDataset) -> std::io::Result<T>,
{
    // Create integrity guard and verify before
    let mut guard = DatasetIntegrityGuard::new(dataset)?;
    let before_result = guard.verify_before();

    // Fail fast if source is already corrupted
    if !before_result.passed {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            before_result.message,
        ));
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

    #[test]
    fn test_registry_creation() {
        let registry = DatasetRegistry::new();
        // At minimum, beads_rust should be available (we're in it)
        assert!(
            registry.is_available(KnownDataset::BeadsRust),
            "beads_rust dataset should be available"
        );
    }

    #[test]
    fn test_isolated_dataset_copy() {
        let isolated = IsolatedDataset::from_dataset(KnownDataset::BeadsRust)
            .expect("should copy beads_rust");

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

    #[test]
    fn test_source_integrity_check() {
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
        let guard = DatasetIntegrityGuard::new(KnownDataset::BeadsRust)
            .expect("should create guard");

        assert_eq!(guard.dataset_name(), "beads_rust");
        assert!(!guard.original_hash().is_empty());
        assert!(!guard.fully_verified()); // Not yet verified
    }

    #[test]
    fn test_integrity_guard_verify_before() {
        let mut guard = DatasetIntegrityGuard::new(KnownDataset::BeadsRust)
            .expect("should create guard");

        let result = guard.verify_before();
        assert!(result.passed, "Before check failed: {}", result.message);
        assert_eq!(result.original_hash, result.current_hash);
    }

    #[test]
    fn test_integrity_guard_verify_after() {
        let mut guard = DatasetIntegrityGuard::new(KnownDataset::BeadsRust)
            .expect("should create guard");

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
        let mut guard = DatasetIntegrityGuard::new(KnownDataset::BeadsRust)
            .expect("should create guard");

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
        let override_cfg = DatasetOverride::new("/some/path", "test")
            .with_name("custom_name");

        assert_eq!(override_cfg.name, Some("custom_name".to_string()));
    }

    #[test]
    fn test_isolated_from_override() {
        // Use beads_rust as the override source (we know it exists)
        let override_cfg = DatasetOverride::new(
            KnownDataset::BeadsRust.source_path(),
            "testing override with beads_rust",
        )
        .with_name("override_test");

        let isolated = isolated_from_override(&override_cfg)
            .expect("should create isolated from override");

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

        let provenance = DatasetProvenance::from_metadata(metadata.clone());
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

        assert_eq!(provenance.context.get("test_name"), Some(&"my_test".to_string()));
        assert_eq!(provenance.context.get("scenario"), Some(&"basic".to_string()));
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
        let isolated = IsolatedDataset::from_dataset(KnownDataset::BeadsRust)
            .expect("should copy beads_rust");

        // beads_rust should have a git repo, so source_commit should be set
        assert!(
            isolated.metadata.source_commit.is_some(),
            "source_commit should be captured for git repos"
        );
    }

    #[test]
    fn test_metadata_to_json_includes_new_fields() {
        let isolated = IsolatedDataset::from_dataset(KnownDataset::BeadsRust)
            .expect("should copy beads_rust");

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
}
