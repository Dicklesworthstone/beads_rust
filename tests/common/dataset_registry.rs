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
}
