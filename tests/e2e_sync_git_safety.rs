//! Regression tests for sync git safety.
//!
//! These tests verify that `br sync` NEVER:
//! - Executes git commands
//! - Creates commits
//! - Stages changes
//! - Mutates the .git directory
//!
//! This is a critical safety invariant documented in:
//! - beads_rust-0v1.2.4: "Guarantee no git operations are executed by br sync"
//! - beads_rust-0v1.3.3: "Regression test: sync never runs git or creates commits"

#![allow(
    clippy::items_after_statements,
    clippy::format_push_string,
    clippy::too_many_lines
)]

mod common;

use common::cli::{BrWorkspace, run_br};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

fn visit_dir(dir: &Path, base: &Path, hash_map: &mut BTreeMap<String, String>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let rel_path = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();

            if path.is_file() {
                if let Ok(contents) = fs::read(&path) {
                    let mut digest = Sha256::new();
                    digest.update(&contents);
                    let hash = format!("{:x}", digest.finalize());
                    hash_map.insert(rel_path, hash);
                }
            } else if path.is_dir() {
                visit_dir(&path, base, hash_map);
            }
        }
    }
}

/// Compute a hash of all files in a directory (recursively).
/// Returns a map of relative paths to their SHA256 hashes.
fn hash_directory_contents(dir: &Path) -> BTreeMap<String, String> {
    let mut hash_map = BTreeMap::new();

    if !dir.exists() {
        return hash_map;
    }

    visit_dir(dir, dir, &mut hash_map);
    hash_map
}

/// Get git status in a directory (returns empty string if not a git repo).
fn get_git_status(dir: &Path) -> String {
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

/// Get the HEAD commit hash (returns None if no commits or not a git repo).
fn get_head_commit(dir: &Path) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Get count of commits in the repo.
fn get_commit_count(dir: &Path) -> usize {
    Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or(0, |o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(0)
        })
}

/// Initialize a git repo in the workspace with an initial commit.
fn init_git_repo(workspace: &BrWorkspace) {
    // Initialize git
    let init = Command::new("git")
        .args(["init"])
        .current_dir(&workspace.root)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init failed");

    // Configure git user for commits
    let _ = Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&workspace.root)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(&workspace.root)
        .output();

    // Create a source file to simulate a real repo with code
    let src_dir = workspace.root.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(
        src_dir.join("main.rs"),
        "fn main() { println!(\"Hello\"); }",
    )
    .expect("write main.rs");

    // Initial commit
    let _ = Command::new("git")
        .args(["add", "."])
        .current_dir(&workspace.root)
        .output();
    let commit = Command::new("git")
        .args(["commit", "-m", "Initial commit"])
        .current_dir(&workspace.root)
        .output()
        .expect("git commit");
    assert!(commit.status.success(), "initial commit failed");
}

/// Regression test: sync export does not create git commits or mutate .git
#[test]
fn regression_sync_export_does_not_create_commits() {
    let workspace = BrWorkspace::new();

    // Initialize git repo first
    init_git_repo(&workspace);

    // Initialize beads
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create some issues
    let create1 = run_br(&workspace, ["create", "Test issue 1"], "create1");
    assert!(
        create1.status.success(),
        "create1 failed: {}",
        create1.stderr
    );
    let create2 = run_br(&workspace, ["create", "Test issue 2"], "create2");
    assert!(
        create2.status.success(),
        "create2 failed: {}",
        create2.stderr
    );

    // Record git state BEFORE sync
    let commit_before = get_head_commit(&workspace.root);
    let commit_count_before = get_commit_count(&workspace.root);
    let git_status_before = get_git_status(&workspace.root);
    let git_dir_hash_before = hash_directory_contents(&workspace.root.join(".git"));

    // Run sync export
    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_export");
    assert!(sync.status.success(), "sync export failed: {}", sync.stderr);

    // Record git state AFTER sync
    let commit_after = get_head_commit(&workspace.root);
    let commit_count_after = get_commit_count(&workspace.root);
    let git_dir_hash_after = hash_directory_contents(&workspace.root.join(".git"));

    // CRITICAL ASSERTIONS:

    // 1. HEAD commit must not change (no new commits created)
    assert_eq!(
        commit_before, commit_after,
        "SAFETY VIOLATION: sync export created a git commit!\n\
         Before: {commit_before:?}\n\
         After: {commit_after:?}"
    );

    // 2. Commit count must not increase
    assert_eq!(
        commit_count_before, commit_count_after,
        "SAFETY VIOLATION: sync export changed commit count!\n\
         Before: {commit_count_before}\n\
         After: {commit_count_after}"
    );

    // 3. .git directory should be unchanged (allowing for index/lock file changes during reads)
    // Filter out files that git legitimately modifies during read operations
    let filter_transient = |hashes: &BTreeMap<String, String>| -> BTreeMap<String, String> {
        hashes
            .iter()
            .filter(|(k, _)| {
                let is_lock = Path::new(k)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("lock"));
                !is_lock
                    && !k.contains("index")
                    && !k.contains("FETCH_HEAD")
                    && !k.contains("ORIG_HEAD")
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    let filtered_before = filter_transient(&git_dir_hash_before);
    let filtered_after = filter_transient(&git_dir_hash_after);

    // Check for new files in .git (excluding transient)
    for (path, hash) in &filtered_after {
        assert!(
            filtered_before.contains_key(path),
            "SAFETY VIOLATION: sync export created new file in .git/: {path}\n\
             Hash: {hash}"
        );
    }

    // Check for modified files in .git (excluding transient)
    for (path, hash_before) in &filtered_before {
        if let Some(hash_after) = filtered_after.get(path) {
            assert!(
                hash_before == hash_after,
                "SAFETY VIOLATION: sync export modified file in .git/: {path}\n\
                 Before: {hash_before}\n\
                 After: {hash_after}"
            );
        }
    }

    // Log success for verification
    eprintln!(
        "[PASS] sync export did not create commits or mutate .git\n\
         - Commit before: {:?}\n\
         - Commit after: {:?}\n\
         - Status before: {:?}\n\
         - .git files checked: {}",
        commit_before,
        commit_after,
        git_status_before.trim(),
        filtered_after.len()
    );
}

/// Regression test: sync import does not create git commits or mutate .git
#[test]
fn regression_sync_import_does_not_create_commits() {
    let workspace = BrWorkspace::new();

    // Initialize git repo first
    init_git_repo(&workspace);

    // Initialize beads and create an issue
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Original issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Export first
    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    // Record git state BEFORE import
    let commit_before = get_head_commit(&workspace.root);
    let commit_count_before = get_commit_count(&workspace.root);
    let git_dir_hash_before = hash_directory_contents(&workspace.root.join(".git"));

    // Run sync import
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "sync_import",
    );
    assert!(
        import.status.success(),
        "sync import failed: {}",
        import.stderr
    );

    // Record git state AFTER import
    let commit_after = get_head_commit(&workspace.root);
    let commit_count_after = get_commit_count(&workspace.root);
    let git_dir_hash_after = hash_directory_contents(&workspace.root.join(".git"));

    // CRITICAL ASSERTIONS:

    // 1. HEAD commit must not change
    assert_eq!(
        commit_before, commit_after,
        "SAFETY VIOLATION: sync import created a git commit!\n\
         Before: {commit_before:?}\n\
         After: {commit_after:?}"
    );

    // 2. Commit count must not increase
    assert_eq!(
        commit_count_before, commit_count_after,
        "SAFETY VIOLATION: sync import changed commit count!\n\
         Before: {commit_count_before}\n\
         After: {commit_count_after}"
    );

    // 3. .git directory core files unchanged
    let filter_transient = |hashes: &BTreeMap<String, String>| -> BTreeMap<String, String> {
        hashes
            .iter()
            .filter(|(k, _)| {
                let is_lock = Path::new(k)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("lock"));
                !is_lock
                    && !k.contains("index")
                    && !k.contains("FETCH_HEAD")
                    && !k.contains("ORIG_HEAD")
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    let filtered_before = filter_transient(&git_dir_hash_before);
    let filtered_after = filter_transient(&git_dir_hash_after);

    for (path, hash) in &filtered_after {
        assert!(
            filtered_before.contains_key(path),
            "SAFETY VIOLATION: sync import created new file in .git/: {path}"
        );
        assert!(
            filtered_before.get(path) == Some(hash),
            "SAFETY VIOLATION: sync import modified file in .git/: {path}"
        );
    }

    eprintln!(
        "[PASS] sync import did not create commits or mutate .git\n\
         - Commit before: {commit_before:?}\n\
         - Commit after: {commit_after:?}"
    );
}

/// Regression test: full sync cycle does not touch git
#[test]
fn regression_full_sync_cycle_does_not_touch_git() {
    let workspace = BrWorkspace::new();

    // Initialize git repo
    init_git_repo(&workspace);

    // Initialize beads
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create multiple issues with different types
    let _ = run_br(&workspace, ["create", "Bug fix", "-t", "bug"], "create_bug");
    let _ = run_br(
        &workspace,
        ["create", "New feature", "-t", "feature"],
        "create_feature",
    );
    let _ = run_br(
        &workspace,
        ["create", "Documentation", "-t", "docs"],
        "create_docs",
    );

    // Record baseline git state
    let baseline_commit = get_head_commit(&workspace.root);
    let baseline_count = get_commit_count(&workspace.root);
    let baseline_git_hash = hash_directory_contents(&workspace.root.join(".git"));

    // Perform full sync cycle: export -> modify JSONL -> import
    let flush1 = run_br(&workspace, ["sync", "--flush-only"], "flush1");
    assert!(flush1.status.success(), "flush1 failed");

    // Modify JSONL externally (simulate git pull bringing changes)
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let original = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let modified = original.replace("Bug fix", "Critical bug fix");
    fs::write(&jsonl_path, modified).expect("write jsonl");

    // Import modified JSONL
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "import_modified",
    );
    assert!(import.status.success(), "import failed");

    // Export again
    let flush2 = run_br(&workspace, ["sync", "--flush-only", "--force"], "flush2");
    assert!(flush2.status.success(), "flush2 failed");

    // Check sync status
    let status = run_br(&workspace, ["sync", "--status"], "status");
    assert!(status.status.success(), "status failed");

    // Verify git state is unchanged after entire cycle
    let final_commit = get_head_commit(&workspace.root);
    let final_count = get_commit_count(&workspace.root);
    let final_git_hash = hash_directory_contents(&workspace.root.join(".git"));

    assert_eq!(
        baseline_commit, final_commit,
        "SAFETY VIOLATION: full sync cycle created git commits!"
    );

    assert_eq!(
        baseline_count, final_count,
        "SAFETY VIOLATION: full sync cycle changed commit count!"
    );

    // Verify .git directory unchanged (excluding transient files)
    let filter_transient = |hashes: &BTreeMap<String, String>| -> BTreeMap<String, String> {
        hashes
            .iter()
            .filter(|(k, _)| {
                let is_lock = Path::new(k)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("lock"));
                !is_lock && !k.contains("index") && !k.contains("HEAD")
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    let baseline_filtered = filter_transient(&baseline_git_hash);
    let final_filtered = filter_transient(&final_git_hash);

    // Check for unexpected .git mutations
    let mut violations = Vec::new();
    for (path, hash) in &final_filtered {
        match baseline_filtered.get(path) {
            None => violations.push(format!("NEW: {path}")),
            Some(old_hash) if old_hash != hash => violations.push(format!("MODIFIED: {path}")),
            _ => {}
        }
    }

    assert!(
        violations.is_empty(),
        "SAFETY VIOLATION: full sync cycle mutated .git/:\n{}",
        violations.join("\n")
    );

    eprintln!(
        "[PASS] full sync cycle did not touch git\n\
         - Operations: init -> create x3 -> export -> modify -> import -> export -> status\n\
         - Commits unchanged: {:?}\n\
         - .git files verified: {}",
        baseline_commit,
        final_filtered.len()
    );
}

/// Regression test: sync with manifest does not touch git
#[test]
fn regression_sync_manifest_does_not_touch_git() {
    let workspace = BrWorkspace::new();

    // Initialize git repo
    init_git_repo(&workspace);

    // Initialize beads
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Manifest test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Record git state before
    let commit_before = get_head_commit(&workspace.root);

    // Run sync with manifest flag
    let sync = run_br(
        &workspace,
        ["sync", "--flush-only", "--manifest"],
        "sync_manifest",
    );
    assert!(
        sync.status.success(),
        "sync manifest failed: {}",
        sync.stderr
    );

    // Verify manifest was created
    let manifest_path = workspace.root.join(".beads").join(".manifest.json");
    assert!(manifest_path.exists(), "manifest file should be created");

    // Verify git state unchanged
    let commit_after = get_head_commit(&workspace.root);
    assert_eq!(
        commit_before, commit_after,
        "SAFETY VIOLATION: sync --manifest created git commit!"
    );

    eprintln!("[PASS] sync --manifest did not touch git");
}

/// Regression test: verify source files are never touched by sync
#[test]
fn regression_sync_never_touches_source_files() {
    let workspace = BrWorkspace::new();

    // Initialize git repo with source files
    init_git_repo(&workspace);

    // Add more source files
    let src_dir = workspace.root.join("src");
    fs::write(src_dir.join("lib.rs"), "pub fn hello() {}").expect("write lib.rs");
    fs::write(src_dir.join("util.rs"), "pub fn util() {}").expect("write util.rs");

    // Create a Cargo.toml
    fs::write(
        workspace.root.join("Cargo.toml"),
        "[package]\nname = \"test\"\nversion = \"0.1.0\"",
    )
    .expect("write Cargo.toml");

    // Hash all source files before sync
    let source_files = [
        workspace.root.join("src").join("main.rs"),
        workspace.root.join("src").join("lib.rs"),
        workspace.root.join("src").join("util.rs"),
        workspace.root.join("Cargo.toml"),
    ];

    let hashes_before: BTreeMap<_, _> = source_files
        .iter()
        .filter(|p| p.exists())
        .map(|p| {
            let content = fs::read(p).unwrap();
            let mut hasher = Sha256::new();
            hasher.update(&content);
            (p.clone(), format!("{:x}", hasher.finalize()))
        })
        .collect();

    // Initialize beads and perform sync operations
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed");

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success(), "create failed");

    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed");

    let import = run_br(&workspace, ["sync", "--import-only", "--force"], "import");
    assert!(import.status.success(), "import failed");

    // Hash source files after sync
    let hashes_after: BTreeMap<_, _> = source_files
        .iter()
        .filter(|p| p.exists())
        .map(|p| {
            let content = fs::read(p).unwrap();
            let mut hasher = Sha256::new();
            hasher.update(&content);
            (p.clone(), format!("{:x}", hasher.finalize()))
        })
        .collect();

    // Verify no source files were modified
    for (path, hash_before) in &hashes_before {
        let hash_after = hashes_after
            .get(path)
            .unwrap_or_else(|| panic!("Source file deleted: {path:?}"));
        assert_eq!(
            hash_before, hash_after,
            "SAFETY VIOLATION: sync modified source file: {path:?}"
        );
    }

    // Verify no source files were deleted
    assert_eq!(
        hashes_before.len(),
        hashes_after.len(),
        "SAFETY VIOLATION: sync deleted source files!"
    );

    eprintln!(
        "[PASS] sync never touched source files\n\
         - Files verified: {:?}",
        source_files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>()
    );
}

// ============================================================================
// COMPREHENSIVE INTEGRATION TEST: beads_rust-0v1.3.2
// Verifies sync operations only touch allowed files in .beads/
// ============================================================================

/// Files that sync is allowed to modify within `.beads/`.
/// This matches the allowlist in `src/sync/path.rs`.
fn is_allowed_sync_file(rel_path: &str) -> bool {
    // Must be under .beads/
    if !rel_path.starts_with(".beads/") && !rel_path.starts_with(".beads\\") {
        return false;
    }

    // Extract filename
    let filename = Path::new(rel_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // Check exact name matches
    const ALLOWED_EXACT_NAMES: &[&str] = &[".manifest.json", "metadata.json"];
    if ALLOWED_EXACT_NAMES.iter().any(|&name| filename == name) {
        return true;
    }

    // Check extension matches
    const ALLOWED_EXTENSIONS: &[&str] = &[
        "db",        // SQLite database
        "db-wal",    // SQLite WAL
        "db-shm",    // SQLite shared memory
        "jsonl",     // JSONL export
        "jsonl.tmp", // Atomic write temp files
    ];

    for ext in ALLOWED_EXTENSIONS {
        if filename.ends_with(&format!(".{ext}")) {
            return true;
        }
    }

    false
}

/// Represents a complete file tree snapshot for comparison.
#[derive(Debug)]
struct FileTreeSnapshot {
    /// Map of relative path -> (SHA256 hash, file size)
    files: BTreeMap<String, (String, u64)>,
    /// Timestamp when snapshot was taken
    #[allow(dead_code)]
    taken_at: std::time::SystemTime,
}

impl FileTreeSnapshot {
    fn new(root: &Path) -> Self {
        let mut files = BTreeMap::new();
        Self::collect_files(root, root, &mut files);
        Self {
            files,
            taken_at: std::time::SystemTime::now(),
        }
    }

    fn collect_files(dir: &Path, base: &Path, files: &mut BTreeMap<String, (String, u64)>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let rel_path = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();

                // Skip log directory (our test artifacts)
                if rel_path.starts_with("logs") || rel_path.starts_with("logs/") {
                    continue;
                }

                if path.is_file() {
                    if let Ok(contents) = fs::read(&path) {
                        let mut hasher = Sha256::new();
                        hasher.update(&contents);
                        let hash = format!("{:x}", hasher.finalize());
                        let size = contents.len() as u64;
                        files.insert(rel_path, (hash, size));
                    }
                } else if path.is_dir() {
                    Self::collect_files(&path, base, files);
                }
            }
        }
    }

    /// Compare two snapshots and return changes.
    fn diff(&self, after: &Self) -> FileTreeDiff {
        let mut created = Vec::new();
        let mut modified = Vec::new();
        let mut deleted = Vec::new();
        let mut unchanged = Vec::new();

        // Find created and modified files
        for (path, (hash_after, size_after)) in &after.files {
            match self.files.get(path) {
                None => created.push(FileChange {
                    path: path.clone(),
                    hash_before: None,
                    hash_after: Some(hash_after.clone()),
                    size_before: None,
                    size_after: Some(*size_after),
                }),
                Some((hash_before, size_before)) if hash_before != hash_after => {
                    modified.push(FileChange {
                        path: path.clone(),
                        hash_before: Some(hash_before.clone()),
                        hash_after: Some(hash_after.clone()),
                        size_before: Some(*size_before),
                        size_after: Some(*size_after),
                    });
                }
                Some(_) => {
                    unchanged.push(path.clone());
                }
            }
        }

        // Find deleted files
        for (path, (hash_before, size_before)) in &self.files {
            if !after.files.contains_key(path) {
                deleted.push(FileChange {
                    path: path.clone(),
                    hash_before: Some(hash_before.clone()),
                    hash_after: None,
                    size_before: Some(*size_before),
                    size_after: None,
                });
            }
        }

        FileTreeDiff {
            created,
            modified,
            deleted,
            unchanged,
        }
    }
}

/// Represents a file change between snapshots.
#[derive(Debug)]
struct FileChange {
    path: String,
    hash_before: Option<String>,
    hash_after: Option<String>,
    size_before: Option<u64>,
    size_after: Option<u64>,
}

impl FileChange {
    fn format_detail(&self) -> String {
        match (&self.hash_before, &self.hash_after) {
            (None, Some(h)) => format!(
                "  CREATED: {} (size: {} bytes, hash: {}...)",
                self.path,
                self.size_after.unwrap_or(0),
                &h[..16.min(h.len())]
            ),
            (Some(h), None) => format!(
                "  DELETED: {} (was {} bytes, hash: {}...)",
                self.path,
                self.size_before.unwrap_or(0),
                &h[..16.min(h.len())]
            ),
            (Some(hb), Some(ha)) => format!(
                "  MODIFIED: {} ({} -> {} bytes)\n    Before: {}...\n    After:  {}...",
                self.path,
                self.size_before.unwrap_or(0),
                self.size_after.unwrap_or(0),
                &hb[..16.min(hb.len())],
                &ha[..16.min(ha.len())]
            ),
            (None, None) => format!("  UNKNOWN: {}", self.path),
        }
    }
}

/// Complete diff between two file tree snapshots.
#[derive(Debug)]
struct FileTreeDiff {
    created: Vec<FileChange>,
    modified: Vec<FileChange>,
    deleted: Vec<FileChange>,
    unchanged: Vec<String>,
}

impl FileTreeDiff {
    #[allow(dead_code)]
    fn has_changes(&self) -> bool {
        !self.created.is_empty() || !self.modified.is_empty() || !self.deleted.is_empty()
    }

    /// Check if all changes are to allowed files.
    /// Returns (violations, `allowed_changes`).
    fn check_allowed_changes(&self) -> (Vec<&FileChange>, Vec<&FileChange>) {
        let mut violations = Vec::new();
        let mut allowed = Vec::new();

        for change in &self.created {
            if is_allowed_sync_file(&change.path) {
                allowed.push(change);
            } else {
                violations.push(change);
            }
        }

        for change in &self.modified {
            if is_allowed_sync_file(&change.path) {
                allowed.push(change);
            } else {
                violations.push(change);
            }
        }

        for change in &self.deleted {
            // Deletions outside .beads are always violations
            if is_allowed_sync_file(&change.path) {
                allowed.push(change);
            } else {
                violations.push(change);
            }
        }

        (violations, allowed)
    }

    /// Generate a detailed log of all changes.
    fn format_log(&self) -> String {
        let mut log = String::new();

        if !self.created.is_empty() {
            log.push_str(&format!("\n=== CREATED FILES ({}) ===\n", self.created.len()));
            for change in &self.created {
                log.push_str(&change.format_detail());
                log.push('\n');
            }
        }

        if !self.modified.is_empty() {
            log.push_str(&format!(
                "\n=== MODIFIED FILES ({}) ===\n",
                self.modified.len()
            ));
            for change in &self.modified {
                log.push_str(&change.format_detail());
                log.push('\n');
            }
        }

        if !self.deleted.is_empty() {
            log.push_str(&format!("\n=== DELETED FILES ({}) ===\n", self.deleted.len()));
            for change in &self.deleted {
                log.push_str(&change.format_detail());
                log.push('\n');
            }
        }

        if log.is_empty() {
            log.push_str("No file changes detected.\n");
        }

        log.push_str(&format!(
            "\n=== SUMMARY ===\n\
             Created: {}\n\
             Modified: {}\n\
             Deleted: {}\n\
             Unchanged: {}\n",
            self.created.len(),
            self.modified.len(),
            self.deleted.len(),
            self.unchanged.len()
        ));

        log
    }
}

/// Integration test: sync export/import only touches allowed files.
///
/// This test implements beads_rust-0v1.3.2:
/// - Creates a temp repo with source files in various directories
/// - Takes complete file tree snapshot before sync
/// - Runs sync export and import operations
/// - Takes complete file tree snapshot after
/// - Verifies ONLY allowed .beads files changed
/// - Captures detailed logs for postmortem on failure
#[test]
fn integration_sync_only_touches_allowed_files() {
    let workspace = BrWorkspace::new();

    // Create a realistic project structure
    create_realistic_project(&workspace);

    // Initialize beads
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create several issues to ensure JSONL has content
    let _ = run_br(
        &workspace,
        ["create", "Feature: User authentication", "-t", "feature", "-p", "1"],
        "create_feature",
    );
    let _ = run_br(
        &workspace,
        ["create", "Bug: Login fails on mobile", "-t", "bug", "-p", "0"],
        "create_bug",
    );
    let _ = run_br(
        &workspace,
        ["create", "Task: Write unit tests", "-t", "task"],
        "create_task",
    );
    let _ = run_br(
        &workspace,
        ["create", "Docs: Update README", "-t", "docs", "-p", "3"],
        "create_docs",
    );

    // =========================================================================
    // TEST 1: Export operation
    // =========================================================================

    eprintln!("\n[TEST 1] Testing sync export...");

    // Take snapshot BEFORE export
    let snapshot_before_export = FileTreeSnapshot::new(&workspace.root);
    eprintln!(
        "  Snapshot before export: {} files",
        snapshot_before_export.files.len()
    );

    // Run sync export
    let export = run_br(&workspace, ["sync", "--flush-only"], "sync_export");
    assert!(
        export.status.success(),
        "sync export failed: {}\nLog: {}",
        export.stderr,
        fs::read_to_string(&export.log_path).unwrap_or_default()
    );

    // Take snapshot AFTER export
    let snapshot_after_export = FileTreeSnapshot::new(&workspace.root);
    eprintln!(
        "  Snapshot after export: {} files",
        snapshot_after_export.files.len()
    );

    // Compare snapshots
    let diff_export = snapshot_before_export.diff(&snapshot_after_export);
    let (violations_export, allowed_export) = diff_export.check_allowed_changes();

    // Write detailed log for export phase
    let export_log = format!(
        "=== SYNC EXPORT PHASE ===\n\
         Command: br sync --flush-only\n\
         Status: {}\n\
         Duration: {:?}\n\n\
         {}\n\n\
         ALLOWED CHANGES:\n{}\n\n\
         VIOLATIONS:\n{}",
        export.status,
        export.duration,
        diff_export.format_log(),
        if allowed_export.is_empty() {
            "  (none)".to_string()
        } else {
            allowed_export
                .iter()
                .map(|c| c.format_detail())
                .collect::<Vec<_>>()
                .join("\n")
        },
        if violations_export.is_empty() {
            "  (none)".to_string()
        } else {
            violations_export
                .iter()
                .map(|c| c.format_detail())
                .collect::<Vec<_>>()
                .join("\n")
        }
    );

    let export_log_path = workspace.log_dir.join("sync_export_diff.log");
    fs::write(&export_log_path, &export_log).expect("write export log");

    // CRITICAL ASSERTION: No violations in export
    assert!(
        violations_export.is_empty(),
        "SAFETY VIOLATION: sync export modified files outside allowed list!\n\n\
         {}\n\n\
         Detailed log: {}",
        violations_export
            .iter()
            .map(|c| c.format_detail())
            .collect::<Vec<_>>()
            .join("\n"),
        export_log_path.display()
    );

    eprintln!(
        "  [PASS] Export modified {} allowed files, 0 violations",
        allowed_export.len()
    );

    // =========================================================================
    // TEST 2: Import operation
    // =========================================================================

    eprintln!("\n[TEST 2] Testing sync import...");

    // Modify the JSONL to simulate external changes (like git pull)
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    if jsonl_path.exists() {
        let original = fs::read_to_string(&jsonl_path).expect("read jsonl");
        let modified = original.replace("User authentication", "User auth v2");
        fs::write(&jsonl_path, modified).expect("write modified jsonl");
    }

    // Take snapshot BEFORE import
    let snapshot_before_import = FileTreeSnapshot::new(&workspace.root);
    eprintln!(
        "  Snapshot before import: {} files",
        snapshot_before_import.files.len()
    );

    // Run sync import
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "sync_import",
    );
    assert!(
        import.status.success(),
        "sync import failed: {}\nLog: {}",
        import.stderr,
        fs::read_to_string(&import.log_path).unwrap_or_default()
    );

    // Take snapshot AFTER import
    let snapshot_after_import = FileTreeSnapshot::new(&workspace.root);
    eprintln!(
        "  Snapshot after import: {} files",
        snapshot_after_import.files.len()
    );

    // Compare snapshots
    let diff_import = snapshot_before_import.diff(&snapshot_after_import);
    let (violations_import, allowed_import) = diff_import.check_allowed_changes();

    // Write detailed log for import phase
    let import_log = format!(
        "=== SYNC IMPORT PHASE ===\n\
         Command: br sync --import-only --force\n\
         Status: {}\n\
         Duration: {:?}\n\n\
         {}\n\n\
         ALLOWED CHANGES:\n{}\n\n\
         VIOLATIONS:\n{}",
        import.status,
        import.duration,
        diff_import.format_log(),
        if allowed_import.is_empty() {
            "  (none)".to_string()
        } else {
            allowed_import
                .iter()
                .map(|c| c.format_detail())
                .collect::<Vec<_>>()
                .join("\n")
        },
        if violations_import.is_empty() {
            "  (none)".to_string()
        } else {
            violations_import
                .iter()
                .map(|c| c.format_detail())
                .collect::<Vec<_>>()
                .join("\n")
        }
    );

    let import_log_path = workspace.log_dir.join("sync_import_diff.log");
    fs::write(&import_log_path, &import_log).expect("write import log");

    // CRITICAL ASSERTION: No violations in import
    assert!(
        violations_import.is_empty(),
        "SAFETY VIOLATION: sync import modified files outside allowed list!\n\n\
         {}\n\n\
         Detailed log: {}",
        violations_import
            .iter()
            .map(|c| c.format_detail())
            .collect::<Vec<_>>()
            .join("\n"),
        import_log_path.display()
    );

    eprintln!(
        "  [PASS] Import modified {} allowed files, 0 violations",
        allowed_import.len()
    );

    // =========================================================================
    // TEST 3: Full sync cycle
    // =========================================================================

    eprintln!("\n[TEST 3] Testing full sync cycle...");

    // Take snapshot BEFORE full cycle
    let snapshot_before_cycle = FileTreeSnapshot::new(&workspace.root);

    // Create more issues, run multiple sync operations
    let _ = run_br(
        &workspace,
        ["create", "Chore: Update deps", "-t", "chore"],
        "create_chore",
    );
    let _ = run_br(&workspace, ["sync", "--flush-only"], "cycle_flush1");
    let _ = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "cycle_import",
    );
    let _ = run_br(
        &workspace,
        ["sync", "--flush-only", "--force"],
        "cycle_flush2",
    );

    // Take snapshot AFTER full cycle
    let snapshot_after_cycle = FileTreeSnapshot::new(&workspace.root);

    // Compare
    let diff_cycle = snapshot_before_cycle.diff(&snapshot_after_cycle);
    let (violations_cycle, allowed_cycle) = diff_cycle.check_allowed_changes();

    // Write detailed log for cycle
    let cycle_log = format!(
        "=== FULL SYNC CYCLE ===\n\
         Operations: create -> flush -> import -> flush\n\n\
         {}\n\n\
         ALLOWED CHANGES:\n{}\n\n\
         VIOLATIONS:\n{}",
        diff_cycle.format_log(),
        if allowed_cycle.is_empty() {
            "  (none)".to_string()
        } else {
            allowed_cycle
                .iter()
                .map(|c| c.format_detail())
                .collect::<Vec<_>>()
                .join("\n")
        },
        if violations_cycle.is_empty() {
            "  (none)".to_string()
        } else {
            violations_cycle
                .iter()
                .map(|c| c.format_detail())
                .collect::<Vec<_>>()
                .join("\n")
        }
    );

    let cycle_log_path = workspace.log_dir.join("sync_cycle_diff.log");
    fs::write(&cycle_log_path, &cycle_log).expect("write cycle log");

    // CRITICAL ASSERTION: No violations in full cycle
    assert!(
        violations_cycle.is_empty(),
        "SAFETY VIOLATION: full sync cycle modified files outside allowed list!\n\n\
         {}\n\n\
         Detailed log: {}",
        violations_cycle
            .iter()
            .map(|c| c.format_detail())
            .collect::<Vec<_>>()
            .join("\n"),
        cycle_log_path.display()
    );

    eprintln!(
        "  [PASS] Full cycle modified {} allowed files, 0 violations",
        allowed_cycle.len()
    );

    // =========================================================================
    // Final summary
    // =========================================================================

    eprintln!(
        "\n[PASS] Integration test: sync only touches allowed files\n\
         - Export: {} allowed changes, 0 violations\n\
         - Import: {} allowed changes, 0 violations\n\
         - Full cycle: {} allowed changes, 0 violations\n\
         - Total files in workspace: {}\n\
         - Logs available in: {}",
        allowed_export.len(),
        allowed_import.len(),
        allowed_cycle.len(),
        snapshot_after_cycle.files.len(),
        workspace.log_dir.display()
    );
}

/// Create a realistic project structure for testing.
fn create_realistic_project(workspace: &BrWorkspace) {
    // Source files
    let src_dir = workspace.root.join("src");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(
        src_dir.join("main.rs"),
        "fn main() {\n    println!(\"Hello, world!\");\n}\n",
    )
    .expect("write main.rs");
    fs::write(
        src_dir.join("lib.rs"),
        "pub mod utils;\npub mod models;\n",
    )
    .expect("write lib.rs");

    // Nested source directories
    let utils_dir = src_dir.join("utils");
    fs::create_dir_all(&utils_dir).expect("create utils dir");
    fs::write(utils_dir.join("mod.rs"), "pub mod helpers;\n").expect("write utils/mod.rs");
    fs::write(
        utils_dir.join("helpers.rs"),
        "pub fn helper() -> i32 { 42 }\n",
    )
    .expect("write helpers.rs");

    let models_dir = src_dir.join("models");
    fs::create_dir_all(&models_dir).expect("create models dir");
    fs::write(models_dir.join("mod.rs"), "pub struct User { name: String }\n")
        .expect("write models/mod.rs");

    // Test files
    let tests_dir = workspace.root.join("tests");
    fs::create_dir_all(&tests_dir).expect("create tests dir");
    fs::write(
        tests_dir.join("integration_tests.rs"),
        "#[test]\nfn test_something() { assert!(true); }\n",
    )
    .expect("write integration_tests.rs");

    // Configuration files
    fs::write(
        workspace.root.join("Cargo.toml"),
        "[package]\nname = \"test-project\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");

    fs::write(workspace.root.join(".gitignore"), "/target\n").expect("write .gitignore");

    // Documentation
    let docs_dir = workspace.root.join("docs");
    fs::create_dir_all(&docs_dir).expect("create docs dir");
    fs::write(docs_dir.join("README.md"), "# Test Project\n\nThis is a test.\n")
        .expect("write docs/README.md");
    fs::write(
        docs_dir.join("API.md"),
        "# API Reference\n\n## Functions\n\n- `helper()`: Returns 42\n",
    )
    .expect("write API.md");

    // Hidden files (not .beads)
    fs::write(
        workspace.root.join(".editorconfig"),
        "root = true\n\n[*]\nindent_style = space\n",
    )
    .expect("write .editorconfig");

    // Data files
    let data_dir = workspace.root.join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");
    fs::write(
        data_dir.join("config.json"),
        "{\"version\": 1, \"enabled\": true}\n",
    )
    .expect("write config.json");
    fs::write(
        data_dir.join("sample.csv"),
        "id,name,value\n1,foo,100\n2,bar,200\n",
    )
    .expect("write sample.csv");

    // Assets
    let assets_dir = workspace.root.join("assets");
    fs::create_dir_all(&assets_dir).expect("create assets dir");
    // Create a small binary file (PNG header simulation)
    let png_header = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    fs::write(assets_dir.join("logo.png"), png_header).expect("write logo.png");

    eprintln!(
        "Created realistic project structure with {} source files",
        count_files(&workspace.root)
    );
}

fn count_files(dir: &Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                count += 1;
            } else if path.is_dir() {
                count += count_files(&path);
            }
        }
    }
    count
}

/// Integration test: sync with manifest touches only allowed files.
#[test]
fn integration_sync_manifest_only_touches_allowed_files() {
    let workspace = BrWorkspace::new();

    // Create project structure
    create_realistic_project(&workspace);

    // Initialize beads
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed");

    let _ = run_br(&workspace, ["create", "Test issue"], "create");

    // Take snapshot before manifest sync
    let snapshot_before = FileTreeSnapshot::new(&workspace.root);

    // Run sync with manifest
    let sync = run_br(
        &workspace,
        ["sync", "--flush-only", "--manifest"],
        "sync_manifest",
    );
    assert!(sync.status.success(), "sync manifest failed: {}", sync.stderr);

    // Take snapshot after
    let snapshot_after = FileTreeSnapshot::new(&workspace.root);

    // Compare
    let diff = snapshot_before.diff(&snapshot_after);
    let (violations, allowed) = diff.check_allowed_changes();

    // Log details
    let log = format!(
        "=== SYNC MANIFEST TEST ===\n\n{}\n",
        diff.format_log()
    );
    let log_path = workspace.log_dir.join("sync_manifest_diff.log");
    fs::write(&log_path, &log).expect("write log");

    assert!(
        violations.is_empty(),
        "SAFETY VIOLATION: sync --manifest modified files outside allowed list!\n\n\
         {}\n\n\
         Log: {}",
        violations
            .iter()
            .map(|c| c.format_detail())
            .collect::<Vec<_>>()
            .join("\n"),
        log_path.display()
    );

    // Verify manifest was actually created
    let manifest_exists = workspace.root.join(".beads").join(".manifest.json").exists();
    assert!(manifest_exists, "Manifest file should have been created");

    eprintln!(
        "[PASS] sync --manifest only touched {} allowed files",
        allowed.len()
    );
}
