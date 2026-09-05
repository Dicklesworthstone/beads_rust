//! Synthetic scale-up benchmark suite for stress testing with large datasets.
//!
//! This module generates synthetic datasets (100k+ issues) by expanding patterns
//! from real datasets, then exercises list/search/ready/graph/sync operations at scale.
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

use beads_rust::coordination::{AgentMailAgentSnapshot, AgentMailReservationSnapshot};
use beads_rust::franken_sync::Connection;
use beads_rust::model::{Comment, Dependency, DependencyType, Issue, IssueType, Priority, Status};
use beads_rust::storage::SqliteStorage;
use beads_rust::util::hex_encode;
use chrono::Utc;
use common::binary_discovery::discover_binaries;
use common::dataset_registry::KnownDataset;
use fsqlite_types::SqliteValue;
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;
use tempfile::TempDir;
use toon_rust::options::ExpandPathsMode;
use toon_rust::{DecodeOptions, try_decode as parse_toon};

// =============================================================================
// Configuration
// =============================================================================

/// Check if stress tests are enabled.
fn stress_tests_enabled() -> bool {
    std::env::var("BR_E2E_STRESS").is_ok()
}

/// Check if the manual million-issue profile is enabled.
fn million_profile_enabled() -> bool {
    std::env::var("BR_SYNTHETIC_MILLION").is_ok()
}

fn synthetic_seed_from_env(default_seed: u64) -> u64 {
    std::env::var("BR_SYNTHETIC_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(default_seed)
}

fn synthetic_evidence_issue_count_from_env(default_count: usize) -> usize {
    std::env::var("BR_SYNTHETIC_EVIDENCE_ISSUES")
        .ok()
        .and_then(|count| count.parse().ok())
        .filter(|count| *count > 0)
        .unwrap_or(default_count)
}

fn synthetic_evidence_output_dir() -> PathBuf {
    std::env::var_os("BR_SYNTHETIC_EVIDENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/benchmark-results"))
}

fn coordination_evidence_output_dir() -> PathBuf {
    std::env::var_os("BR_COORDINATION_EVIDENCE_DIR")
        .or_else(|| std::env::var_os("BR_SYNTHETIC_EVIDENCE_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests/artifacts/perf/coordination-large-swarm-20260508"))
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
    /// 1,000,000 issues - manual 256GB+/64-core profile
    Million,
}

impl ScaleTier {
    #[must_use]
    pub const fn issue_count(self) -> usize {
        match self {
            Self::Small => 10_000,
            Self::Medium => 50_000,
            Self::Large => 100_000,
            Self::XLarge => 250_000,
            Self::Million => 1_000_000,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Small => "small_10k",
            Self::Medium => "medium_50k",
            Self::Large => "large_100k",
            Self::XLarge => "xlarge_250k",
            Self::Million => "million_1m",
        }
    }

    /// Target dependency density (deps per issue on average).
    #[must_use]
    pub const fn dependency_density(self) -> f64 {
        match self {
            Self::Small => 0.3,
            Self::Medium | Self::Large => 0.5,
            Self::XLarge => 0.7,
            Self::Million => 0.9,
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
    /// Number of label names in the synthetic label pool.
    pub label_pool_size: usize,
    /// Minimum labels assigned per issue.
    pub min_labels_per_issue: usize,
    /// Maximum labels assigned per issue.
    pub max_labels_per_issue: usize,
    /// Probability that an issue receives comments.
    pub comment_density: f64,
    /// Maximum comments per commented issue.
    pub max_comments_per_issue: usize,
    /// Simulated agent identities available for claims and comments.
    pub simulated_agent_count: usize,
    /// Probability that an issue is claimed by a simulated agent.
    pub claim_density: f64,
    /// Bias dependencies toward low-numbered hub issues for skewed DAG profiles.
    pub dag_skew: f64,
}

impl SyntheticConfig {
    #[must_use]
    pub fn from_tier(tier: ScaleTier) -> Self {
        Self {
            issue_count: tier.issue_count(),
            dependency_density: tier.dependency_density(),
            seed: 42, // Reproducible by default
            base_dataset: Some(KnownDataset::BeadsRust),
            label_pool_size: 64,
            min_labels_per_issue: 0,
            max_labels_per_issue: 4,
            comment_density: 0.15,
            max_comments_per_issue: 3,
            simulated_agent_count: 10_000,
            claim_density: 0.05,
            dag_skew: 1.25,
        }
    }

    #[must_use]
    pub const fn ci_profile(seed: u64) -> Self {
        Self {
            issue_count: 256,
            dependency_density: 0.4,
            seed,
            base_dataset: None,
            label_pool_size: 12,
            min_labels_per_issue: 0,
            max_labels_per_issue: 3,
            comment_density: 0.25,
            max_comments_per_issue: 2,
            simulated_agent_count: 16,
            claim_density: 0.2,
            dag_skew: 0.8,
        }
    }

    #[must_use]
    pub const fn million_agent_profile(seed: u64) -> Self {
        Self {
            issue_count: ScaleTier::Million.issue_count(),
            dependency_density: ScaleTier::Million.dependency_density(),
            seed,
            base_dataset: Some(KnownDataset::BeadsRust),
            label_pool_size: 512,
            min_labels_per_issue: 1,
            max_labels_per_issue: 6,
            comment_density: 0.2,
            max_comments_per_issue: 4,
            simulated_agent_count: 10_000,
            claim_density: 0.08,
            dag_skew: 1.8,
        }
    }

    #[must_use]
    pub const fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    #[must_use]
    pub const fn with_issue_count(mut self, issue_count: usize) -> Self {
        self.issue_count = issue_count;
        self
    }

    #[must_use]
    pub const fn with_label_distribution(
        mut self,
        label_pool_size: usize,
        min_labels_per_issue: usize,
        max_labels_per_issue: usize,
    ) -> Self {
        self.label_pool_size = label_pool_size;
        self.min_labels_per_issue = min_labels_per_issue;
        self.max_labels_per_issue = max_labels_per_issue;
        self
    }

    #[must_use]
    pub const fn with_comment_distribution(
        mut self,
        comment_density: f64,
        max_comments_per_issue: usize,
    ) -> Self {
        self.comment_density = comment_density;
        self.max_comments_per_issue = max_comments_per_issue;
        self
    }

    #[must_use]
    pub const fn with_agent_distribution(
        mut self,
        simulated_agent_count: usize,
        claim_density: f64,
    ) -> Self {
        self.simulated_agent_count = simulated_agent_count;
        self.claim_density = claim_density;
        self
    }

    #[must_use]
    pub const fn with_dag_skew(mut self, dag_skew: f64) -> Self {
        self.dag_skew = dag_skew;
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
    /// Total labels assigned across all issues.
    pub label_assignment_count: usize,
    /// Total comments generated across all issues.
    pub comment_count: usize,
    /// Number of simulated agent identities in the corpus.
    pub simulated_agent_count: usize,
    /// Number of claimed issues assigned to simulated agents.
    pub claim_count: usize,
    /// How the generated JSONL was loaded into SQLite for this profile.
    pub load_strategy: String,
    /// Generation duration
    pub generation_ms: u128,
    /// JSONL file size in bytes
    pub jsonl_size_bytes: u64,
    /// Byte count predicted by the generator while streaming JSONL.
    pub expected_jsonl_size_bytes: u64,
    /// DB file size after rebuild
    pub db_size_bytes: u64,
    /// SHA-256 hash of generated issues.jsonl.
    pub content_hash: String,
    /// Health checks recorded after import/rebuild.
    pub health: GenerationHealth,
}

/// Health evidence for a generated corpus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationHealth {
    /// Every JSONL line parsed as an Issue.
    pub jsonl_valid: bool,
    /// Number of valid JSONL issue records.
    pub jsonl_issue_count: usize,
    /// `br sync --import-only --json` ran and succeeded.
    pub sync_import_ok: bool,
    /// `br doctor --json` ran and succeeded after import.
    pub doctor_ok: bool,
    /// `br sync --status --json` reported no dirty DB/JSONL divergence.
    pub sync_status_clean: bool,
}

/// Reproducibility manifest for a generated synthetic corpus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntheticCorpusManifest {
    pub schema_version: String,
    pub generator: String,
    pub generated_at: String,
    pub config: SyntheticConfigSnapshot,
    pub metrics: GenerationMetrics,
    pub reproduction_command: String,
}

/// Serializable subset of SyntheticConfig.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntheticConfigSnapshot {
    pub issue_count: usize,
    pub dependency_density: f64,
    pub seed: u64,
    pub base_dataset: Option<String>,
    pub label_pool_size: usize,
    pub min_labels_per_issue: usize,
    pub max_labels_per_issue: usize,
    pub comment_density: f64,
    pub max_comments_per_issue: usize,
    pub simulated_agent_count: usize,
    pub claim_density: f64,
    pub dag_skew: f64,
}

/// A generated synthetic dataset in an isolated workspace.
pub struct SyntheticDataset {
    pub temp_dir: TempDir,
    pub root: PathBuf,
    pub beads_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub config: SyntheticConfig,
    pub metrics: GenerationMetrics,
}

fn configure_benchmark_environment(command: &mut Command, root: &Path) {
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if ["BR_", "BD_", "BEADS_", "TOON_", "RCH_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            command.env_remove(key);
        }
    }
    command
        .current_dir(root)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join(".config"))
        .env("XDG_CACHE_HOME", root.join(".cache"))
        .env("XDG_STATE_HOME", root.join(".local/state"))
        .env("BEADS_DIR", root.join(".beads"))
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "error")
        .env("PATH", common::cli::deduplicated_br_path());
}

fn synthetic_command(binary: &Path, root: &Path) -> Command {
    let mut command = Command::new(binary);
    configure_benchmark_environment(&mut command, root);
    command
}

impl SyntheticDataset {
    /// Generate a synthetic dataset based on the config.
    ///
    /// # Errors
    ///
    /// Returns an error if the temporary workspace or any CLI command fails.
    pub fn generate(config: SyntheticConfig, br_path: &Path) -> std::io::Result<Self> {
        let start = Instant::now();
        let temp_dir = TempDir::new_in(common::cli::isolated_temp_root())?;
        let root = temp_dir.path().to_path_buf();
        let beads_dir = root.join(".beads");
        let manifest_path = root.join("synthetic-corpus-manifest.json");

        // Create minimal git scaffold
        fs::create_dir_all(root.join(".git"))?;
        fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n")?;

        // Initialize beads
        let init_output = synthetic_command(br_path, &root)
            .args(["init"])
            .current_dir(&root)
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "error")
            .env("PATH", common::cli::deduplicated_br_path())
            .output()?;

        if !init_output.status.success() {
            return Err(std::io::Error::other(format!(
                "br init failed: {}",
                String::from_utf8_lossy(&init_output.stderr)
            )));
        }

        let generated = write_synthetic_jsonl(&config, &beads_dir.join("issues.jsonl"))?;
        let sync_import_ok = run_br_status(
            br_path,
            ["sync", "--import-only", "--json"],
            &root,
            "br sync --import-only",
        )?;
        let doctor_ok = doctor_workspace_is_healthy(br_path, &root)?;
        let sync_status_clean = sync_status_is_clean(br_path, &root)?;
        let jsonl_health = validate_generated_jsonl(&beads_dir.join("issues.jsonl"))?;

        let generation_ms = start.elapsed().as_millis();
        let db_path = beads_dir.join("beads.db");
        let db_size_bytes = fs::metadata(&db_path).map_or(0, |m| m.len());

        let metrics = GenerationMetrics {
            issue_count: generated.issue_count,
            dependency_count: generated.dependency_count,
            label_assignment_count: generated.label_assignment_count,
            comment_count: generated.comment_count,
            simulated_agent_count: config.simulated_agent_count,
            claim_count: generated.claim_count,
            load_strategy: "sync_import".to_string(),
            generation_ms,
            jsonl_size_bytes: generated.jsonl_size_bytes,
            expected_jsonl_size_bytes: generated.expected_jsonl_size_bytes,
            db_size_bytes,
            content_hash: generated.content_hash,
            health: GenerationHealth {
                jsonl_valid: jsonl_health.valid,
                jsonl_issue_count: jsonl_health.issue_count,
                sync_import_ok,
                doctor_ok,
                sync_status_clean,
            },
        };

        let manifest = SyntheticCorpusManifest {
            schema_version: "br.synthetic-corpus.v1".to_string(),
            generator: "bench_synthetic_scale::write_synthetic_jsonl".to_string(),
            generated_at: Utc::now().to_rfc3339(),
            config: SyntheticConfigSnapshot::from(&config),
            metrics: metrics.clone(),
            reproduction_command: reproduction_command_for(&config),
        };
        write_json_pretty(&manifest_path, &manifest)?;

        Ok(Self {
            temp_dir,
            root,
            beads_dir,
            manifest_path,
            config,
            metrics,
        })
    }

    /// Generate a synthetic dataset and seed SQLite directly from the same
    /// deterministic JSONL stream.
    ///
    /// This keeps coordination-status evidence focused on the read path under
    /// test instead of spending most of the run inside JSONL import.
    ///
    /// # Errors
    ///
    /// Returns an error if the temporary workspace, JSONL generation, or direct
    /// SQLite load fails.
    pub fn generate_direct_sqlite(
        config: SyntheticConfig,
        br_path: &Path,
    ) -> std::io::Result<Self> {
        let start = Instant::now();
        let temp_dir = TempDir::new_in(common::cli::isolated_temp_root())?;
        let root = temp_dir.path().to_path_buf();
        let beads_dir = root.join(".beads");
        let manifest_path = root.join("synthetic-corpus-manifest.json");

        fs::create_dir_all(root.join(".git"))?;
        fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n")?;

        let init_output = synthetic_command(br_path, &root)
            .args(["init"])
            .current_dir(&root)
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "error")
            .env("PATH", common::cli::deduplicated_br_path())
            .output()?;

        if !init_output.status.success() {
            return Err(std::io::Error::other(format!(
                "br init failed: {}",
                String::from_utf8_lossy(&init_output.stderr)
            )));
        }

        let jsonl_path = beads_dir.join("issues.jsonl");
        let generated = write_synthetic_jsonl(&config, &jsonl_path)?;
        populate_sqlite_direct(&beads_dir.join("beads.db"), &jsonl_path)?;
        let storage = sqlite_io(SqliteStorage::open(&beads_dir.join("beads.db")))?;
        let persisted_issue_count = sqlite_io(storage.count_issues())?;
        let jsonl_health = validate_generated_jsonl(&jsonl_path)?;
        if persisted_issue_count != generated.issue_count {
            return Err(std::io::Error::other(format!(
                "direct SQLite seed had {persisted_issue_count} issues after reopen, expected {}",
                generated.issue_count
            )));
        }

        let generation_ms = start.elapsed().as_millis();
        let db_path = beads_dir.join("beads.db");
        let db_size_bytes = fs::metadata(&db_path).map_or(0, |m| m.len());

        let metrics = GenerationMetrics {
            issue_count: generated.issue_count,
            dependency_count: generated.dependency_count,
            label_assignment_count: generated.label_assignment_count,
            comment_count: generated.comment_count,
            simulated_agent_count: config.simulated_agent_count,
            claim_count: generated.claim_count,
            load_strategy: "direct_sqlite_seed".to_string(),
            generation_ms,
            jsonl_size_bytes: generated.jsonl_size_bytes,
            expected_jsonl_size_bytes: generated.expected_jsonl_size_bytes,
            db_size_bytes,
            content_hash: generated.content_hash,
            health: GenerationHealth {
                jsonl_valid: jsonl_health.valid,
                jsonl_issue_count: jsonl_health.issue_count,
                sync_import_ok: false,
                doctor_ok: false,
                sync_status_clean: false,
            },
        };

        let manifest = SyntheticCorpusManifest {
            schema_version: "br.synthetic-corpus.v1".to_string(),
            generator: "bench_synthetic_scale::write_synthetic_jsonl+populate_sqlite_direct"
                .to_string(),
            generated_at: Utc::now().to_rfc3339(),
            config: SyntheticConfigSnapshot::from(&config),
            metrics: metrics.clone(),
            reproduction_command: coordination_evidence_reproduction_command_for(&config),
        };
        write_json_pretty(&manifest_path, &manifest)?;

        Ok(Self {
            temp_dir,
            root,
            beads_dir,
            manifest_path,
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

impl From<&SyntheticConfig> for SyntheticConfigSnapshot {
    fn from(config: &SyntheticConfig) -> Self {
        Self {
            issue_count: config.issue_count,
            dependency_density: config.dependency_density,
            seed: config.seed,
            base_dataset: config
                .base_dataset
                .map(|dataset| dataset.name().to_string()),
            label_pool_size: config.label_pool_size,
            min_labels_per_issue: config.min_labels_per_issue,
            max_labels_per_issue: config.max_labels_per_issue,
            comment_density: config.comment_density,
            max_comments_per_issue: config.max_comments_per_issue,
            simulated_agent_count: config.simulated_agent_count,
            claim_density: config.claim_density,
            dag_skew: config.dag_skew,
        }
    }
}

struct GeneratedCorpusStats {
    issue_count: usize,
    dependency_count: usize,
    label_assignment_count: usize,
    comment_count: usize,
    claim_count: usize,
    jsonl_size_bytes: u64,
    expected_jsonl_size_bytes: u64,
    content_hash: String,
}

struct JsonlHealth {
    valid: bool,
    issue_count: usize,
}

fn write_synthetic_jsonl(
    config: &SyntheticConfig,
    jsonl_path: &Path,
) -> std::io::Result<GeneratedCorpusStats> {
    let file = File::create(jsonl_path)?;
    let mut writer = BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut rng = StdRng::seed_from_u64(config.seed);

    let mut stats = GeneratedCorpusStats {
        issue_count: 0,
        dependency_count: 0,
        label_assignment_count: 0,
        comment_count: 0,
        claim_count: 0,
        jsonl_size_bytes: 0,
        expected_jsonl_size_bytes: 0,
        content_hash: String::new(),
    };

    for index in 0..config.issue_count {
        let issue = generate_synthetic_issue(config, &mut rng, index, &mut stats);
        let line = serde_json::to_vec(&issue)?;
        writer.write_all(&line)?;
        writer.write_all(b"\n")?;
        hasher.update(&line);
        hasher.update(b"\n");
        stats.expected_jsonl_size_bytes = stats
            .expected_jsonl_size_bytes
            .saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX))
            .saturating_add(1);
        stats.issue_count += 1;

        if stats.issue_count.is_multiple_of(100_000) {
            eprintln!(
                "  Streamed {}/{} synthetic issues...",
                stats.issue_count, config.issue_count
            );
        }
    }
    writer.flush()?;

    stats.jsonl_size_bytes = fs::metadata(jsonl_path).map_or(0, |metadata| metadata.len());
    stats.content_hash = hex_encode(&hasher.finalize());
    Ok(stats)
}

fn generate_synthetic_issue(
    config: &SyntheticConfig,
    rng: &mut StdRng,
    index: usize,
    stats: &mut GeneratedCorpusStats,
) -> Issue {
    let id = synthetic_issue_id(index);
    let created_at = synthetic_timestamp(index);
    let labels = generate_labels(config, rng);
    let dependencies = generate_dependencies(config, rng, index, &id, created_at);
    let comments = generate_comments(config, rng, index, &id, created_at, stats.comment_count);
    let assignee = choose_claimed_agent(config, rng);

    stats.dependency_count += dependencies.len();
    stats.label_assignment_count += labels.len();
    stats.comment_count += comments.len();
    if assignee.is_some() {
        stats.claim_count += 1;
    }

    Issue {
        id,
        title: generate_title(rng, index),
        description: Some(format!(
            "Synthetic scale corpus issue {index}; seed={}; agents={}; generated for br large-workspace benchmarking.",
            config.seed, config.simulated_agent_count
        )),
        status: if assignee.is_some() {
            Status::InProgress
        } else {
            Status::Open
        },
        priority: Priority(rng.random_range(0..=4)),
        issue_type: synthetic_issue_type(rng),
        assignee,
        created_at,
        created_by: Some(synthetic_agent_name(
            config.seed,
            index % effective_agent_count(config),
        )),
        updated_at: created_at,
        source_repo: Some("synthetic-swarm-corpus".to_string()),
        compaction_level: Some(0),
        original_size: Some(0),
        labels,
        dependencies,
        comments,
        ..Issue::default()
    }
}

fn synthetic_issue_id(index: usize) -> String {
    format!("synth-{index:08x}")
}

fn synthetic_timestamp(index: usize) -> chrono::DateTime<Utc> {
    let base = chrono::DateTime::<Utc>::UNIX_EPOCH;
    base + chrono::Duration::seconds(usize_to_i64(index % 86_400))
}

fn synthetic_issue_type(rng: &mut StdRng) -> IssueType {
    match rng.random_range(0..10) {
        0..=5 => IssueType::Task,
        6..=7 => IssueType::Bug,
        8 => IssueType::Feature,
        _ => IssueType::Chore,
    }
}

fn effective_agent_count(config: &SyntheticConfig) -> usize {
    config.simulated_agent_count.max(1)
}

fn synthetic_agent_name(seed: u64, index: usize) -> String {
    format!("agent-{seed:016x}-{index:05}")
}

fn choose_claimed_agent(config: &SyntheticConfig, rng: &mut StdRng) -> Option<String> {
    if config.claim_density <= 0.0 {
        return None;
    }
    if rng.random_range(0.0..1.0) >= config.claim_density.min(1.0) {
        return None;
    }
    Some(synthetic_agent_name(
        config.seed,
        rng.random_range(0..effective_agent_count(config)),
    ))
}

fn generate_labels(config: &SyntheticConfig, rng: &mut StdRng) -> Vec<String> {
    if config.label_pool_size == 0 || config.max_labels_per_issue == 0 {
        return Vec::new();
    }

    let min = config.min_labels_per_issue.min(config.max_labels_per_issue);
    let max = config.max_labels_per_issue.max(min);
    let label_count = rng.random_range(min..=max).min(config.label_pool_size);
    let mut labels = BTreeSet::new();

    while labels.len() < label_count {
        labels.insert(format!(
            "label-{:03}",
            rng.random_range(0..config.label_pool_size)
        ));
    }

    labels.into_iter().collect()
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn generate_dependencies(
    config: &SyntheticConfig,
    rng: &mut StdRng,
    index: usize,
    issue_id: &str,
    created_at: chrono::DateTime<Utc>,
) -> Vec<Dependency> {
    if index == 0 || config.dependency_density <= 0.0 {
        return Vec::new();
    }

    let guaranteed = config.dependency_density.floor() as usize;
    let fractional = config.dependency_density.fract();
    let dep_count = guaranteed + usize::from(rng.random_range(0.0..1.0) < fractional);
    let dep_count = dep_count.min(index);
    let mut targets = BTreeSet::new();

    while targets.len() < dep_count {
        targets.insert(skewed_dependency_target(config, rng, index));
    }

    targets
        .into_iter()
        .map(|target| Dependency {
            issue_id: issue_id.to_string(),
            depends_on_id: synthetic_issue_id(target),
            dep_type: DependencyType::Blocks,
            created_at,
            created_by: Some("synthetic-corpus-generator".to_string()),
            metadata: Some(format!(
                "{{\"seed\":{},\"dag_skew\":{}}}",
                config.seed, config.dag_skew
            )),
            thread_id: None,
        })
        .collect()
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn skewed_dependency_target(
    config: &SyntheticConfig,
    rng: &mut StdRng,
    upper_bound: usize,
) -> usize {
    if upper_bound <= 1 {
        return 0;
    }

    let skew = config.dag_skew.max(0.0);
    let sample = rng.random_range(0.0..1.0_f64).powf(1.0 + skew);
    ((sample * upper_bound as f64).floor() as usize).min(upper_bound - 1)
}

fn generate_comments(
    config: &SyntheticConfig,
    rng: &mut StdRng,
    index: usize,
    issue_id: &str,
    created_at: chrono::DateTime<Utc>,
    next_comment_id: usize,
) -> Vec<Comment> {
    if config.comment_density <= 0.0 || config.max_comments_per_issue == 0 {
        return Vec::new();
    }
    if rng.random_range(0.0..1.0) >= config.comment_density.min(1.0) {
        return Vec::new();
    }

    let comment_count = rng.random_range(1..=config.max_comments_per_issue);
    (0..comment_count)
        .map(|offset| Comment {
            id: usize_to_i64(next_comment_id + offset + 1),
            issue_id: issue_id.to_string(),
            author: synthetic_agent_name(
                config.seed,
                rng.random_range(0..effective_agent_count(config)),
            ),
            body: format!(
                "Synthetic agent note {offset} for issue {index}; seed={}; reproducible benchmark corpus.",
                config.seed
            ),
            created_at: created_at + chrono::Duration::seconds(usize_to_i64(offset + 1)),
        })
        .collect()
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn validate_generated_jsonl(jsonl_path: &Path) -> std::io::Result<JsonlHealth> {
    let content = fs::read_to_string(jsonl_path)?;
    let mut issue_count = 0;
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        serde_json::from_str::<Issue>(line).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("generated JSONL issue line is invalid: {err}"),
            )
        })?;
        issue_count += 1;
    }

    Ok(JsonlHealth {
        valid: true,
        issue_count,
    })
}

fn sqlite_io<T, E: std::fmt::Display>(result: std::result::Result<T, E>) -> std::io::Result<T> {
    result.map_err(|err| std::io::Error::other(err.to_string()))
}

fn populate_sqlite_direct(db_path: &Path, jsonl_path: &Path) -> std::io::Result<()> {
    {
        let _storage = sqlite_io(SqliteStorage::open(db_path))?;
    }
    let conn = sqlite_io(Connection::open(db_path.to_string_lossy().into_owned()))?;
    sqlite_io(conn.execute("PRAGMA synchronous=OFF"))?;
    sqlite_io(conn.execute("BEGIN IMMEDIATE"))?;

    let file = File::open(jsonl_path)?;
    let reader = BufReader::new(file);
    let mut issue_count = 0usize;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let issue = serde_json::from_str::<Issue>(&line).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("generated JSONL issue line is invalid: {err}"),
            )
        })?;
        insert_issue_direct(&conn, &issue)?;
        issue_count += 1;

        if issue_count.is_multiple_of(100_000) {
            eprintln!("  Seeded {issue_count} synthetic issues into SQLite...");
        }
    }

    sqlite_io(conn.execute("COMMIT"))?;
    let row = sqlite_io(conn.query_row("SELECT count(*) FROM issues"))?;
    let persisted_count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
    let persisted_count = usize::try_from(persisted_count).unwrap_or(0);
    if persisted_count != issue_count {
        return Err(std::io::Error::other(format!(
            "direct SQLite seed persisted {persisted_count} issues, expected {issue_count}"
        )));
    }
    Ok(())
}

fn optional_text(value: Option<&str>) -> SqliteValue {
    value.map_or(SqliteValue::Null, SqliteValue::from)
}

fn optional_datetime(value: Option<chrono::DateTime<Utc>>) -> SqliteValue {
    value.map_or(SqliteValue::Null, |timestamp| {
        SqliteValue::from(timestamp.to_rfc3339())
    })
}

fn insert_issue_direct(conn: &Connection, issue: &Issue) -> std::io::Result<()> {
    let content_hash = issue
        .content_hash
        .clone()
        .unwrap_or_else(|| issue.compute_content_hash());
    sqlite_io(conn.execute_with_params(
        "INSERT INTO issues (
            id, content_hash, title, description, design, acceptance_criteria, notes,
            status, priority, issue_type, assignee, owner, estimated_minutes,
            created_at, created_by, updated_at, closed_at, close_reason,
            closed_by_session, due_at, defer_until, external_ref, source_system,
            source_repo, deleted_at, deleted_by, delete_reason, original_type,
            compaction_level, compacted_at, compacted_at_commit, original_size,
            sender, ephemeral, pinned, is_template
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            SqliteValue::from(issue.id.as_str()),
            SqliteValue::from(content_hash.as_str()),
            SqliteValue::from(issue.title.as_str()),
            SqliteValue::from(issue.description.as_deref().unwrap_or("")),
            SqliteValue::from(issue.design.as_deref().unwrap_or("")),
            SqliteValue::from(issue.acceptance_criteria.as_deref().unwrap_or("")),
            SqliteValue::from(issue.notes.as_deref().unwrap_or("")),
            SqliteValue::from(issue.status.as_str()),
            SqliteValue::from(issue.priority.0),
            SqliteValue::from(issue.issue_type.as_str()),
            optional_text(issue.assignee.as_deref()),
            SqliteValue::from(issue.owner.as_deref().unwrap_or("")),
            issue.estimated_minutes.map_or(SqliteValue::Null, SqliteValue::from),
            SqliteValue::from(issue.created_at.to_rfc3339()),
            SqliteValue::from(issue.created_by.as_deref().unwrap_or("")),
            SqliteValue::from(issue.updated_at.to_rfc3339()),
            optional_datetime(issue.closed_at),
            SqliteValue::from(issue.close_reason.as_deref().unwrap_or("")),
            SqliteValue::from(issue.closed_by_session.as_deref().unwrap_or("")),
            optional_datetime(issue.due_at),
            optional_datetime(issue.defer_until),
            optional_text(issue.external_ref.as_deref()),
            SqliteValue::from(issue.source_system.as_deref().unwrap_or("")),
            SqliteValue::from(issue.source_repo.as_deref().unwrap_or(".")),
            optional_datetime(issue.deleted_at),
            SqliteValue::from(issue.deleted_by.as_deref().unwrap_or("")),
            SqliteValue::from(issue.delete_reason.as_deref().unwrap_or("")),
            SqliteValue::from(issue.original_type.as_deref().unwrap_or("")),
            SqliteValue::from(i64::from(issue.compaction_level.unwrap_or(0))),
            optional_datetime(issue.compacted_at),
            optional_text(issue.compacted_at_commit.as_deref()),
            SqliteValue::from(i64::from(issue.original_size.unwrap_or(0))),
            SqliteValue::from(issue.sender.as_deref().unwrap_or("")),
            SqliteValue::from(i64::from(i32::from(issue.ephemeral))),
            SqliteValue::from(i64::from(i32::from(issue.pinned))),
            SqliteValue::from(i64::from(i32::from(issue.is_template))),
        ],
    ))?;

    for label in &issue.labels {
        sqlite_io(conn.execute_with_params(
            "INSERT OR IGNORE INTO labels (issue_id, label) VALUES (?, ?)",
            &[
                SqliteValue::from(issue.id.as_str()),
                SqliteValue::from(label.as_str()),
            ],
        ))?;
    }

    for dependency in &issue.dependencies {
        sqlite_io(conn.execute_with_params(
            "INSERT OR IGNORE INTO dependencies (
                issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
             ) VALUES (?, ?, ?, ?, ?, ?, ?)",
            &[
                SqliteValue::from(issue.id.as_str()),
                SqliteValue::from(dependency.depends_on_id.as_str()),
                SqliteValue::from(dependency.dep_type.as_str()),
                SqliteValue::from(dependency.created_at.to_rfc3339()),
                SqliteValue::from(dependency.created_by.as_deref().unwrap_or("synthetic")),
                SqliteValue::from(dependency.metadata.as_deref().unwrap_or("{}")),
                SqliteValue::from(dependency.thread_id.as_deref().unwrap_or("")),
            ],
        ))?;
    }

    for comment in &issue.comments {
        sqlite_io(conn.execute_with_params(
            "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
            &[
                SqliteValue::from(issue.id.as_str()),
                SqliteValue::from(comment.author.as_str()),
                SqliteValue::from(comment.body.as_str()),
                SqliteValue::from(comment.created_at.to_rfc3339()),
            ],
        ))?;
    }

    Ok(())
}

fn run_br_status<const N: usize>(
    br_path: &Path,
    args: [&str; N],
    workspace: &Path,
    label: &str,
) -> std::io::Result<bool> {
    let output = synthetic_command(br_path, workspace)
        .args(args)
        .current_dir(workspace)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "error")
        // A host with `br` in more than one $PATH directory makes every
        // spawned `br doctor` emit a `br_path_dupes` WARN, failing corpus
        // generation for a reason unrelated to the workspace under test.
        // Mirror tests/e2e_doctor_fixture_suite.rs and drop later
        // duplicates only.
        .env("PATH", common::cli::deduplicated_br_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if output.status.success() {
        return Ok(true);
    }

    Err(std::io::Error::other(format!(
        "{label} failed: stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )))
}

fn doctor_workspace_is_healthy(br_path: &Path, workspace: &Path) -> std::io::Result<bool> {
    let output = synthetic_command(br_path, workspace)
        .args(["doctor", "--json", "--no-auto-import", "--no-auto-flush"])
        .current_dir(workspace)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "error")
        .env("PATH", common::cli::deduplicated_br_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(std::io::Error::other(format!(
            "br doctor failed: stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let report: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        std::io::Error::other(format!(
            "br doctor emitted invalid JSON: {error}; stdout={}",
            String::from_utf8_lossy(&output.stdout)
        ))
    })?;
    let checks = report
        .get("checks")
        .and_then(Value::as_array)
        .ok_or_else(|| std::io::Error::other("br doctor JSON omitted checks"))?;
    let workspace_healthy = report
        .get("workspace_health")
        .and_then(Value::as_str)
        .is_some_and(|health| health == "healthy");
    let has_error = checks
        .iter()
        .any(|check| check.get("status").and_then(Value::as_str) == Some("error"));

    Ok(workspace_healthy && !has_error)
}

fn sync_status_is_clean(br_path: &Path, workspace: &Path) -> std::io::Result<bool> {
    let output = synthetic_command(br_path, workspace)
        .args(["sync", "--status", "--json"])
        .current_dir(workspace)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "error")
        .env("PATH", common::cli::deduplicated_br_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "br sync --status failed: stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid br sync --status JSON: {err}"),
        )
    })?;

    Ok(value
        .get("dirty_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(u64::MAX)
        == 0
        && !value
            .get("jsonl_newer")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
        && !value
            .get("db_newer")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true))
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let file = File::create(path)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer_pretty(writer, value)?;
    Ok(())
}

fn reproduction_command_for(config: &SyntheticConfig) -> String {
    if config.issue_count >= ScaleTier::Million.issue_count() {
        format!(
            "BR_E2E_STRESS=1 BR_SYNTHETIC_MILLION=1 BR_SYNTHETIC_SEED={} cargo test --test bench_synthetic_scale stress_synthetic_million -- --ignored --nocapture",
            config.seed
        )
    } else {
        format!(
            "BR_E2E_STRESS=1 BR_SYNTHETIC_SEED={} cargo test --test bench_synthetic_scale -- --ignored --nocapture",
            config.seed
        )
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
    /// SHA-256 of stdout for output identity evidence.
    pub stdout_sha256: Option<String>,
    /// Error message if failed
    pub error: Option<String>,
}

/// Full benchmark results for a synthetic dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntheticBenchmark {
    /// Scale tier name
    pub tier: String,
    /// Dataset configuration used for reproducibility.
    pub config: SyntheticConfigSnapshot,
    /// Dataset generation metrics
    pub generation: GenerationMetrics,
    /// Operation benchmarks
    pub operations: Vec<OperationMetrics>,
    /// Summary statistics
    pub summary: BenchmarkSummary,
    /// `br` binary path measured by the benchmark.
    pub br_binary_path: String,
    /// Command that can reproduce this benchmark profile.
    pub reproduction_command: String,
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

const SYNTHETIC_FULL_GRAPH_MAX_ISSUES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SyntheticWorkloadSpec {
    operation: &'static str,
    args: Vec<String>,
}

fn synthetic_graph_workloads(issue_count: usize) -> Vec<SyntheticWorkloadSpec> {
    let hot_hub = synthetic_issue_id(0);
    let hot_leaf = synthetic_issue_id(issue_count.saturating_sub(1));

    let mut workloads = vec![
        SyntheticWorkloadSpec {
            operation: "graph_hot_hub",
            args: vec!["graph".to_string(), hot_hub, "--json".to_string()],
        },
        SyntheticWorkloadSpec {
            operation: "dep_tree_hot_leaf",
            args: vec![
                "dep".to_string(),
                "tree".to_string(),
                hot_leaf,
                "--direction".to_string(),
                "down".to_string(),
                "--max-depth".to_string(),
                "12".to_string(),
                "--json".to_string(),
            ],
        },
    ];

    if issue_count <= SYNTHETIC_FULL_GRAPH_MAX_ISSUES {
        workloads.push(SyntheticWorkloadSpec {
            operation: "graph_all_components",
            args: vec![
                "graph".to_string(),
                "--all".to_string(),
                "--json".to_string(),
            ],
        });
    }

    workloads
}

/// Run a command and capture metrics.
fn run_operation(
    br_path: &Path,
    args: &[&str],
    workspace: &Path,
    operation: &str,
) -> OperationMetrics {
    let start = Instant::now();

    let output = run_measured_br_command(br_path, args, workspace);

    let duration = start.elapsed();

    match output {
        Ok(out) => {
            let MeasuredCommandOutput {
                stdout,
                stderr,
                success,
                peak_rss_bytes,
            } = out;
            let stdout_sha256 = if success {
                Some(sha256_hex(&stdout))
            } else {
                None
            };
            let error = if success { None } else { Some(stderr) };

            OperationMetrics {
                operation: operation.to_string(),
                duration_ms: duration.as_millis(),
                peak_rss_bytes,
                success,
                output_size_bytes: stdout.len(),
                stdout_sha256,
                error,
            }
        }
        Err(e) => OperationMetrics {
            operation: operation.to_string(),
            duration_ms: duration.as_millis(),
            peak_rss_bytes: None,
            success: false,
            output_size_bytes: 0,
            stdout_sha256: None,
            error: Some(e.to_string()),
        },
    }
}

struct MeasuredCommandOutput {
    stdout: Vec<u8>,
    stderr: String,
    success: bool,
    peak_rss_bytes: Option<u64>,
}

fn run_measured_br_command(
    br_path: &Path,
    args: &[&str],
    workspace: &Path,
) -> std::io::Result<MeasuredCommandOutput> {
    if Path::new("/usr/bin/time").is_file() {
        let output = Command::new("/usr/bin/time") // ubs:ignore - benchmark harness intentionally invokes GNU time for child RSS
            .arg("-v")
            .arg(br_path)
            .args(args)
            .current_dir(workspace)
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "error")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Ok(MeasuredCommandOutput {
            stdout: output.stdout,
            peak_rss_bytes: parse_time_max_rss_bytes(&stderr),
            stderr,
            success: output.status.success(),
        });
    }

    let output = Command::new(br_path) // ubs:ignore - benchmark harness executes only discovered br binaries
        .args(args)
        .current_dir(workspace)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "error")
        .env("PATH", common::cli::deduplicated_br_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    Ok(MeasuredCommandOutput {
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        success: output.status.success(),
        peak_rss_bytes: None,
    })
}

fn parse_time_max_rss_bytes(stderr: &str) -> Option<u64> {
    stderr.lines().find_map(|line| {
        let kb = line
            .trim_start()
            .strip_prefix("Maximum resident set size (kbytes):")?
            .trim()
            .parse::<u64>()
            .ok()?;
        Some(kb.saturating_mul(1024))
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
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

    for workload in synthetic_graph_workloads(dataset.config.issue_count) {
        let args: Vec<&str> = workload.args.iter().map(String::as_str).collect();
        operations.push(run_operation(br_path, &args, workspace, workload.operation));
    }

    // Export operation
    operations.push(run_operation(
        br_path,
        &["sync", "--flush-only", "--json"],
        workspace,
        "sync_flush",
    ));

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
                n if n < 1_000_000 => "xlarge",
                _ => "million",
            }
        ),
        config: SyntheticConfigSnapshot::from(&dataset.config),
        generation: dataset.metrics.clone(),
        operations,
        summary,
        br_binary_path: br_path.display().to_string(),
        reproduction_command: reproduction_command_for(&dataset.config),
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
        "Dataset: {} issues, {} dependencies, {} labels, {} comments, {} claims across {} agents ({:.1} KB JSONL, {:.1} KB DB)",
        generation.issue_count,
        generation.dependency_count,
        generation.label_assignment_count,
        generation.comment_count,
        generation.claim_count,
        generation.simulated_agent_count,
        generation.jsonl_size_bytes as f64 / 1024.0,
        generation.db_size_bytes as f64 / 1024.0
    );
    println!("Generation time: {}ms", generation.generation_ms);
    println!("JSONL hash: {}", generation.content_hash);
    println!(
        "Health: jsonl_valid={} sync_import_ok={} doctor_ok={} sync_status_clean={}",
        generation.health.jsonl_valid,
        generation.health.sync_import_ok,
        generation.health.doctor_ok,
        generation.health.sync_status_clean
    );
    println!("{dash}");

    // Operations
    println!(
        "{:<20} {:>12} {:>12} {:>12} {:>10}",
        "Operation", "Duration(ms)", "Output(KB)", "RSS(MB)", "Status"
    );
    println!("{dash}");

    for op in &benchmark.operations {
        let status = if op.success { "OK" } else { "FAIL" };
        let output_kb = op.output_size_bytes as f64 / 1024.0;
        let rss_mb = op.peak_rss_bytes.map_or_else(
            || "n/a".to_string(),
            |bytes| format!("{:.1}", bytes as f64 / (1024.0 * 1024.0)),
        );
        println!(
            "{:<20} {:>12} {:>12.1} {:>12} {:>10}",
            op.operation, op.duration_ms, output_kb, rss_mb, status
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

fn evidence_reproduction_command_for(config: &SyntheticConfig) -> String {
    format!(
        "BR_E2E_STRESS=1 BR_SYNTHETIC_SEED={} BR_SYNTHETIC_EVIDENCE_ISSUES={} cargo test --test bench_synthetic_scale stress_synthetic_evidence_profile -- --ignored --nocapture",
        config.seed, config.issue_count
    )
}

fn coordination_evidence_reproduction_command_for(config: &SyntheticConfig) -> String {
    format!(
        "BR_E2E_STRESS=1 BR_SYNTHETIC_SEED={} BR_SYNTHETIC_EVIDENCE_ISSUES={} cargo test --test bench_synthetic_scale stress_coordination_large_swarm_evidence -- --ignored --nocapture",
        config.seed, config.issue_count
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClaimedIssue {
    id: String,
    assignee: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CoordinationSnapshotMetrics {
    agents_path: String,
    reservations_path: String,
    agent_rows: usize,
    active_reservation_rows: usize,
    expired_reservation_rows: usize,
    agent_snapshot_sha256: String,
    reservation_snapshot_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CoordinationOperationMetrics {
    operation: String,
    command: String,
    duration_ms: u128,
    peak_rss_bytes: Option<u64>,
    success: bool,
    output_size_bytes: usize,
    stdout_sha256: Option<String>,
    normalized_stdout_sha256: Option<String>,
    total_claims: Option<u64>,
    active_reservation_matches: usize,
    expired_reservation_matches: usize,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CoordinationComparison {
    json_normalized_sha256: String,
    toon_normalized_sha256: String,
    semantic_hashes_match: bool,
    json_output_size_bytes: usize,
    toon_output_size_bytes: usize,
    json_duration_ms: u128,
    toon_duration_ms: u128,
    rss_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CoordinationGuardrail {
    latest_comment_limit: usize,
    bounded_comment_rows_upper_bound: usize,
    generated_comment_rows: usize,
    baseline: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CoordinationLargeSwarmEvidence {
    schema_version: String,
    generated_at: String,
    br_binary_path: String,
    reproduction_command: String,
    config: SyntheticConfigSnapshot,
    generation: GenerationMetrics,
    snapshots: CoordinationSnapshotMetrics,
    operations: Vec<CoordinationOperationMetrics>,
    comparison: CoordinationComparison,
    guardrail: CoordinationGuardrail,
}

fn collect_claimed_issues(jsonl_path: &Path) -> std::io::Result<Vec<ClaimedIssue>> {
    let file = File::open(jsonl_path)?;
    let reader = BufReader::new(file);
    let mut claimed = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let issue = serde_json::from_str::<Issue>(&line).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("generated JSONL issue line is invalid: {err}"),
            )
        })?;
        if issue.status == Status::InProgress
            && let Some(assignee) = issue
                .assignee
                .as_deref()
                .map(str::trim)
                .filter(|assignee| !assignee.is_empty())
        {
            claimed.push(ClaimedIssue {
                id: issue.id,
                assignee: assignee.to_string(),
            });
        }
    }

    Ok(claimed)
}

fn fixed_utc_timestamp(timestamp: &str) -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .expect("fixed timestamp should parse")
        .with_timezone(&Utc)
}

fn write_jsonl_rows<T: Serialize>(path: &Path, rows: &[T]) -> std::io::Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    for row in rows {
        serde_json::to_writer(&mut writer, row)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

fn file_sha256_hex(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    Ok(sha256_hex(&bytes))
}

fn write_coordination_snapshots(
    output_dir: &Path,
    config: &SyntheticConfig,
    claimed: &[ClaimedIssue],
) -> std::io::Result<CoordinationSnapshotMetrics> {
    let agents_path = output_dir.join("coordination-agents.jsonl");
    let reservations_path = output_dir.join("coordination-reservations.jsonl");
    let active_expires = fixed_utc_timestamp("2099-01-01T00:00:00Z");
    let expired_at = fixed_utc_timestamp("2020-01-01T00:00:00Z");
    let agent_last_active = fixed_utc_timestamp("2026-05-08T15:00:00Z");

    let agents = (0..config.simulated_agent_count)
        .map(|index| AgentMailAgentSnapshot {
            name: synthetic_agent_name(config.seed, index),
            task_description: format!(
                "Synthetic coordination evidence agent {index} for seed {}",
                config.seed
            ),
            last_active_ts: agent_last_active,
            contact_policy: "auto".to_string(),
        })
        .collect::<Vec<_>>();

    let mut used_agents = BTreeSet::new();
    let mut active_claims = Vec::new();
    for claim in claimed {
        if used_agents.insert(claim.assignee.clone()) {
            active_claims.push(claim);
            if active_claims.len() == 4_000 {
                break;
            }
        }
    }

    let mut expired_claims = Vec::new();
    for claim in claimed {
        if used_agents.contains(&claim.assignee) {
            continue;
        }
        if used_agents.insert(claim.assignee.clone()) {
            expired_claims.push(claim);
            if expired_claims.len() == 4_000 {
                break;
            }
        }
    }

    let mut reservations = Vec::with_capacity(active_claims.len() + expired_claims.len());

    reservations.extend(
        active_claims
            .iter()
            .map(|claim| AgentMailReservationSnapshot {
                holder: claim.assignee.clone(),
                path_pattern: "src/coordination.rs".to_string(),
                exclusive: true,
                reason: Some(format!("coordination perf active {}", claim.id)),
                expires_ts: active_expires,
                released_ts: None,
                thread_id: Some(claim.id.clone()),
            }),
    );
    reservations.extend(
        expired_claims
            .iter()
            .map(|claim| AgentMailReservationSnapshot {
                holder: claim.assignee.clone(),
                path_pattern: "tests/bench_synthetic_scale.rs".to_string(),
                exclusive: true,
                reason: Some(format!("coordination perf expired {}", claim.id)),
                expires_ts: expired_at,
                released_ts: Some(expired_at),
                thread_id: Some(claim.id.clone()),
            }),
    );

    write_jsonl_rows(&agents_path, &agents)?;
    write_jsonl_rows(&reservations_path, &reservations)?;

    Ok(CoordinationSnapshotMetrics {
        agents_path: agents_path.display().to_string(),
        reservations_path: reservations_path.display().to_string(),
        agent_rows: agents.len(),
        active_reservation_rows: active_claims.len(),
        expired_reservation_rows: expired_claims.len(),
        agent_snapshot_sha256: file_sha256_hex(&agents_path)?,
        reservation_snapshot_sha256: file_sha256_hex(&reservations_path)?,
    })
}

fn normalize_coordination_output(format: &str, stdout: &[u8]) -> std::io::Result<Value> {
    let mut value = if format == "toon" {
        let raw = std::str::from_utf8(stdout).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("coordination TOON output was not UTF-8: {err}"),
            )
        })?;
        let decode_options = DecodeOptions {
            indent: None,
            strict: None,
            expand_paths: Some(ExpandPathsMode::Safe),
        };
        Value::from(parse_toon(raw.trim(), Some(decode_options)).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("coordination TOON output did not decode: {err}"),
            )
        })?)
    } else {
        serde_json::from_slice(stdout).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("coordination JSON output did not parse: {err}"),
            )
        })?
    };

    normalize_coordination_value(&mut value);
    Ok(value)
}

#[test]
fn coordination_normalization_expands_safe_toon_key_folding() {
    let toon = br#"schema_version: br.coordination.v1
generated_at: "2026-05-08T00:00:00Z"
summary:
  total_claims: 1
claims[1]:
  - assessment:
      updated_age_minutes: 42
      reservation.state: active
"#;

    let normalized =
        normalize_coordination_output("toon", toon).expect("TOON normalization should decode");

    assert_eq!(
        normalized.pointer("/claims/0/assessment/reservation/state"),
        Some(&Value::String("active".to_string()))
    );
    assert_eq!(reservation_state_count(&normalized, "active"), 1);
}

fn normalize_coordination_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "generated_at" {
                    *child = Value::String("<normalized>".to_string());
                } else if key == "updated_age_minutes" {
                    *child = Value::Number(0.into());
                } else {
                    normalize_coordination_value(child);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_coordination_value(item);
            }
        }
        Value::String(text) => {
            *text = normalize_age_minutes_text(text);
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn normalize_age_minutes_text(text: &str) -> String {
    let marker = "age_minutes=";
    let mut output = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(index) = rest.find(marker) {
        let (before, after_before) = rest.split_at(index);
        output.push_str(before);
        output.push_str(marker);
        output.push_str("<normalized>");
        let Some(after_marker) = after_before.strip_prefix(marker) else {
            output.push_str(after_before);
            return output;
        };
        let digit_count = after_marker.bytes().take_while(u8::is_ascii_digit).count();
        rest = after_marker.get(digit_count..).unwrap_or("");
    }

    output.push_str(rest);
    output
}

fn normalized_value_sha256(value: &Value) -> std::io::Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(sha256_hex(&bytes))
}

fn reservation_state_count(value: &Value, state: &str) -> usize {
    value
        .get("claims")
        .and_then(Value::as_array)
        .map(|claims| {
            claims
                .iter()
                .filter(|claim| {
                    claim
                        .pointer("/assessment/reservation/state")
                        .and_then(Value::as_str)
                        .is_some_and(|actual| actual == state)
                })
                .count()
        })
        .unwrap_or(0)
}

fn coordination_total_claims(value: &Value) -> Option<u64> {
    value
        .pointer("/summary/total_claims")
        .and_then(Value::as_u64)
        .or_else(|| {
            value
                .get("claims")
                .and_then(Value::as_array)
                .and_then(|claims| u64::try_from(claims.len()).ok())
        })
}

fn command_line_for(br_path: &Path, args: &[String]) -> String {
    let mut parts = vec![br_path.display().to_string()];
    parts.extend(args.iter().cloned());
    parts.join(" ")
}

fn run_coordination_status_operation(
    br_path: &Path,
    args: &[String],
    workspace: &Path,
    operation: &str,
    format: &str,
) -> CoordinationOperationMetrics {
    let start = Instant::now();
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = run_measured_br_command(br_path, &arg_refs, workspace);
    let duration_ms = start.elapsed().as_millis();
    let command = command_line_for(br_path, args);

    match output {
        Ok(out) => {
            let stdout_sha256 = out.success.then(|| sha256_hex(&out.stdout));
            if !out.success {
                return CoordinationOperationMetrics {
                    operation: operation.to_string(),
                    command,
                    duration_ms,
                    peak_rss_bytes: out.peak_rss_bytes,
                    success: false,
                    output_size_bytes: out.stdout.len(),
                    stdout_sha256,
                    normalized_stdout_sha256: None,
                    total_claims: None,
                    active_reservation_matches: 0,
                    expired_reservation_matches: 0,
                    error: Some(out.stderr),
                };
            }

            match normalize_coordination_output(format, &out.stdout)
                .and_then(|value| normalized_value_sha256(&value).map(|hash| (value, hash)))
            {
                Ok((value, normalized_hash)) => CoordinationOperationMetrics {
                    operation: operation.to_string(),
                    command,
                    duration_ms,
                    peak_rss_bytes: out.peak_rss_bytes,
                    success: true,
                    output_size_bytes: out.stdout.len(),
                    stdout_sha256,
                    normalized_stdout_sha256: Some(normalized_hash),
                    total_claims: coordination_total_claims(&value),
                    active_reservation_matches: reservation_state_count(&value, "active"),
                    expired_reservation_matches: reservation_state_count(&value, "expired"),
                    error: None,
                },
                Err(err) => CoordinationOperationMetrics {
                    operation: operation.to_string(),
                    command,
                    duration_ms,
                    peak_rss_bytes: out.peak_rss_bytes,
                    success: false,
                    output_size_bytes: out.stdout.len(),
                    stdout_sha256,
                    normalized_stdout_sha256: None,
                    total_claims: None,
                    active_reservation_matches: 0,
                    expired_reservation_matches: 0,
                    error: Some(format!("normalization failed: {err}")),
                },
            }
        }
        Err(err) => CoordinationOperationMetrics {
            operation: operation.to_string(),
            command,
            duration_ms,
            peak_rss_bytes: None,
            success: false,
            output_size_bytes: 0,
            stdout_sha256: None,
            normalized_stdout_sha256: None,
            total_claims: None,
            active_reservation_matches: 0,
            expired_reservation_matches: 0,
            error: Some(err.to_string()),
        },
    }
}

fn write_coordination_summary_notes(
    path: &Path,
    evidence: &CoordinationLargeSwarmEvidence,
) -> std::io::Result<()> {
    let json = evidence
        .operations
        .iter()
        .find(|operation| operation.operation == "coordination_status_json")
        .expect("JSON operation should be recorded");
    let toon = evidence
        .operations
        .iter()
        .find(|operation| operation.operation == "coordination_status_toon")
        .expect("TOON operation should be recorded");
    let summary = format!(
        "# Coordination Large Swarm Evidence\n\n\
Generated at: {}\n\n\
Corpus: {} issues, {} in_progress claims, {} dependencies, {} labels, {} comments, {} simulated agents.\n\n\
Snapshots: {} agents, {} active reservations, {} expired reservations.\n\n\
Commands:\n\n\
- JSON: `{}`\n\
- TOON: `{}`\n\n\
Results:\n\n\
- JSON duration: {} ms; output bytes: {}; raw sha256: {}; normalized sha256: {}; peak RSS bytes: {}\n\
- TOON duration: {} ms; output bytes: {}; raw sha256: {}; normalized sha256: {}; peak RSS bytes: {}\n\
- Semantic hashes match: {}\n\n\
Guardrail: coordination status requested {} latest comment row per in-progress issue. The command-side upper bound was {} comment rows for {} claims, while the generated corpus contained {} total comments. Future changes should keep the command on bounded latest-comment evidence unless they also publish a stronger measured baseline.\n\n\
Reproduce: `{}`\n",
        evidence.generated_at,
        evidence.generation.issue_count,
        evidence.generation.claim_count,
        evidence.generation.dependency_count,
        evidence.generation.label_assignment_count,
        evidence.generation.comment_count,
        evidence.generation.simulated_agent_count,
        evidence.snapshots.agent_rows,
        evidence.snapshots.active_reservation_rows,
        evidence.snapshots.expired_reservation_rows,
        json.command,
        toon.command,
        json.duration_ms,
        json.output_size_bytes,
        json.stdout_sha256.as_deref().unwrap_or("n/a"),
        json.normalized_stdout_sha256.as_deref().unwrap_or("n/a"),
        json.peak_rss_bytes
            .map_or_else(|| "n/a".to_string(), |rss| rss.to_string()),
        toon.duration_ms,
        toon.output_size_bytes,
        toon.stdout_sha256.as_deref().unwrap_or("n/a"),
        toon.normalized_stdout_sha256.as_deref().unwrap_or("n/a"),
        toon.peak_rss_bytes
            .map_or_else(|| "n/a".to_string(), |rss| rss.to_string()),
        evidence.comparison.semantic_hashes_match,
        evidence.guardrail.latest_comment_limit,
        evidence.guardrail.bounded_comment_rows_upper_bound,
        evidence.generation.claim_count,
        evidence.guardrail.generated_comment_rows,
        evidence.reproduction_command,
    );

    fs::write(path, summary)
}

// =============================================================================
// Tests
// =============================================================================

/// Bounded evidence profile for streaming-output RSS/latency artifacts.
/// Env gate: BR_E2E_STRESS=1
#[test]
#[ignore = "stress test: BR_E2E_STRESS=1 cargo test --test bench_synthetic_scale stress_synthetic_evidence_profile -- --ignored --nocapture"]
fn stress_synthetic_evidence_profile() {
    if !stress_tests_enabled() {
        eprintln!("Skipping stress test (set BR_E2E_STRESS=1 to enable)");
        return;
    }

    let binaries = discover_binaries().expect("Binary discovery failed");
    let seed = synthetic_seed_from_env(20_260_503);
    let issue_count = synthetic_evidence_issue_count_from_env(1_024);
    let config = SyntheticConfig::ci_profile(seed)
        .with_issue_count(issue_count)
        .with_label_distribution(32, 1, 4)
        .with_comment_distribution(0.2, 3)
        .with_agent_distribution(10_000, 0.08)
        .with_dag_skew(1.5);

    eprintln!(
        "Generating bounded synthetic evidence dataset ({} issues, {} simulated agents)...",
        config.issue_count, config.simulated_agent_count
    );
    let dataset = SyntheticDataset::generate(config, &binaries.br.path)
        .expect("Failed to generate synthetic evidence dataset");

    eprintln!("Running bounded evidence benchmarks...");
    let mut benchmark = benchmark_synthetic(&dataset, &binaries.br.path);
    benchmark.tier = "synthetic_evidence".to_string();
    benchmark.reproduction_command = evidence_reproduction_command_for(&dataset.config);
    let failed_operations = benchmark
        .operations
        .iter()
        .filter(|operation| !operation.success)
        .map(|operation| operation.operation.as_str())
        .collect::<Vec<_>>();
    assert!(
        failed_operations.is_empty(),
        "bounded evidence profile should have no failed operations: {failed_operations:?}"
    );
    print_benchmark(&benchmark);

    let output_dir = synthetic_evidence_output_dir();
    fs::create_dir_all(&output_dir).expect("create evidence output dir");
    let result_path = output_dir.join("synthetic_evidence_latest.json");
    write_benchmark_json(&[benchmark], &result_path).expect("write evidence benchmark");
    let manifest_path = output_dir.join("synthetic-corpus-manifest.json");
    fs::copy(&dataset.manifest_path, &manifest_path).expect("persist corpus manifest");

    println!("Evidence benchmark written to: {}", result_path.display());
    println!("Corpus manifest written to: {}", manifest_path.display());
}

/// Coordination status evidence profile for a 100k issue / 10k+ claim swarm.
/// Env gate: BR_E2E_STRESS=1
#[test]
#[ignore = "stress test: BR_E2E_STRESS=1 cargo test --test bench_synthetic_scale stress_coordination_large_swarm_evidence -- --ignored --nocapture"]
fn stress_coordination_large_swarm_evidence() {
    if !stress_tests_enabled() {
        eprintln!("Skipping stress test (set BR_E2E_STRESS=1 to enable)");
        return;
    }

    let binaries = discover_binaries().expect("Binary discovery failed");
    let seed = synthetic_seed_from_env(20_260_508);
    let issue_count = synthetic_evidence_issue_count_from_env(100_000);
    let config = SyntheticConfig::ci_profile(seed)
        .with_issue_count(issue_count)
        .with_label_distribution(64, 1, 4)
        .with_comment_distribution(0.25, 3)
        .with_agent_distribution(10_000, 0.2)
        .with_dag_skew(1.5);

    eprintln!(
        "Generating coordination evidence dataset ({} issues, {} simulated agents)...",
        config.issue_count, config.simulated_agent_count
    );
    let dataset = SyntheticDataset::generate_direct_sqlite(config, &binaries.br.path)
        .expect("Failed to generate coordination evidence dataset");
    let claimed = collect_claimed_issues(&dataset.beads_dir.join("issues.jsonl"))
        .expect("collect claimed synthetic issues");

    assert_eq!(dataset.metrics.issue_count, issue_count);
    if issue_count >= 100_000 {
        assert!(
            dataset.metrics.claim_count >= 10_000,
            "coordination evidence corpus must include at least 10k in_progress claims; got {}",
            dataset.metrics.claim_count
        );
    }
    assert_eq!(claimed.len(), dataset.metrics.claim_count);

    let output_dir = coordination_evidence_output_dir();
    fs::create_dir_all(&output_dir).expect("create coordination evidence output dir");
    let snapshots = write_coordination_snapshots(&output_dir, &dataset.config, &claimed)
        .expect("write coordination snapshot fixtures");
    if issue_count >= 100_000 {
        assert!(
            snapshots.active_reservation_rows >= 1_000,
            "snapshot should include thousands of active reservations"
        );
        assert!(
            snapshots.expired_reservation_rows >= 1_000,
            "snapshot should include thousands of expired reservations"
        );
    }

    let latest_comment_limit = 1;
    let reservations_path = output_dir.join("coordination-reservations.jsonl");
    let agents_path = output_dir.join("coordination-agents.jsonl");
    let reservations_arg = fs::canonicalize(&reservations_path)
        .expect("canonicalize reservation snapshot path")
        .display()
        .to_string();
    let agents_arg = fs::canonicalize(&agents_path)
        .expect("canonicalize agent snapshot path")
        .display()
        .to_string();
    let json_args = vec![
        "coordination".to_string(),
        "status".to_string(),
        "--json".to_string(),
        "--no-auto-import".to_string(),
        "--no-auto-flush".to_string(),
        "--owner-kind".to_string(),
        "swarm-agent".to_string(),
        "--comments".to_string(),
        latest_comment_limit.to_string(),
        "--reservations".to_string(),
        reservations_arg.clone(),
        "--agents".to_string(),
        agents_arg.clone(),
    ];
    let toon_args = vec![
        "coordination".to_string(),
        "status".to_string(),
        "--format".to_string(),
        "toon".to_string(),
        "--no-auto-import".to_string(),
        "--no-auto-flush".to_string(),
        "--owner-kind".to_string(),
        "swarm-agent".to_string(),
        "--comments".to_string(),
        latest_comment_limit.to_string(),
        "--reservations".to_string(),
        reservations_arg,
        "--agents".to_string(),
        agents_arg,
    ];

    eprintln!("Running coordination status JSON evidence command...");
    let json_operation = run_coordination_status_operation(
        &binaries.br.path,
        &json_args,
        dataset.workspace_root(),
        "coordination_status_json",
        "json",
    );
    eprintln!("Running coordination status TOON evidence command...");
    let toon_operation = run_coordination_status_operation(
        &binaries.br.path,
        &toon_args,
        dataset.workspace_root(),
        "coordination_status_toon",
        "toon",
    );

    for operation in [&json_operation, &toon_operation] {
        assert!(
            operation.success,
            "{} failed: {:?}",
            operation.operation,
            operation.error.as_deref()
        );
        assert!(
            operation.output_size_bytes > 0,
            "{} should emit measurable output",
            operation.operation
        );
        assert_eq!(
            operation.stdout_sha256.as_deref().map(str::len),
            Some(64),
            "{} should include raw stdout hash evidence",
            operation.operation
        );
        assert_eq!(
            operation.normalized_stdout_sha256.as_deref().map(str::len),
            Some(64),
            "{} should include normalized stdout hash evidence",
            operation.operation
        );
        assert_eq!(
            operation.total_claims,
            Some(u64::try_from(dataset.metrics.claim_count).unwrap_or(u64::MAX)),
            "{} should report every in_progress claim",
            operation.operation
        );
        assert!(
            operation.active_reservation_matches >= snapshots.active_reservation_rows,
            "{} should match active snapshot reservations",
            operation.operation
        );
        assert!(
            operation.expired_reservation_matches >= snapshots.expired_reservation_rows,
            "{} should match expired snapshot reservations",
            operation.operation
        );
    }

    let json_hash = json_operation
        .normalized_stdout_sha256
        .clone()
        .expect("JSON normalized hash");
    let toon_hash = toon_operation
        .normalized_stdout_sha256
        .clone()
        .expect("TOON normalized hash");
    let comparison = CoordinationComparison {
        json_normalized_sha256: json_hash.clone(),
        toon_normalized_sha256: toon_hash.clone(),
        semantic_hashes_match: json_hash == toon_hash,
        json_output_size_bytes: json_operation.output_size_bytes,
        toon_output_size_bytes: toon_operation.output_size_bytes,
        json_duration_ms: json_operation.duration_ms,
        toon_duration_ms: toon_operation.duration_ms,
        rss_available: json_operation.peak_rss_bytes.is_some()
            && toon_operation.peak_rss_bytes.is_some(),
    };

    let evidence = CoordinationLargeSwarmEvidence {
        schema_version: "br.coordination-large-swarm-evidence.v1".to_string(),
        generated_at: Utc::now().to_rfc3339(),
        br_binary_path: binaries.br.path.display().to_string(),
        reproduction_command: coordination_evidence_reproduction_command_for(&dataset.config),
        config: SyntheticConfigSnapshot::from(&dataset.config),
        generation: dataset.metrics.clone(),
        snapshots,
        operations: vec![json_operation, toon_operation],
        comparison,
        guardrail: CoordinationGuardrail {
            latest_comment_limit,
            bounded_comment_rows_upper_bound: dataset
                .metrics
                .claim_count
                .saturating_mul(latest_comment_limit),
            generated_comment_rows: dataset.metrics.comment_count,
            baseline: "coordination status must keep comment loading bounded to the latest relevant rows per in_progress issue; publish a new measured evidence report before raising the bound or returning full history".to_string(),
        },
    };

    let result_path = output_dir.join("coordination_evidence_latest.json");
    write_json_pretty(&result_path, &evidence).expect("write coordination evidence report");
    let manifest_path = output_dir.join("synthetic-corpus-manifest.json");
    fs::copy(&dataset.manifest_path, &manifest_path).expect("persist corpus manifest");
    let notes_path = output_dir.join("notes.md");
    write_coordination_summary_notes(&notes_path, &evidence).expect("write coordination notes");

    println!(
        "Coordination evidence benchmark written to: {}",
        result_path.display()
    );
    println!("Corpus manifest written to: {}", manifest_path.display());
    println!("Summary notes written to: {}", notes_path.display());
}

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

/// Manual million-issue synthetic benchmark with 10,000 simulated agents.
/// Env gate: BR_E2E_STRESS=1 BR_SYNTHETIC_MILLION=1
#[test]
#[ignore = "manual stress test: BR_E2E_STRESS=1 BR_SYNTHETIC_MILLION=1 cargo test --test bench_synthetic_scale stress_synthetic_million -- --ignored --nocapture"]
fn stress_synthetic_million() {
    if !stress_tests_enabled() || !million_profile_enabled() {
        eprintln!(
            "Skipping million-issue stress test (set BR_E2E_STRESS=1 BR_SYNTHETIC_MILLION=1)"
        );
        return;
    }

    let binaries = discover_binaries().expect("Binary discovery failed");
    let seed = synthetic_seed_from_env(42);

    println!("\n=== Synthetic Scale-Up Benchmark: Million (1M / 10K agents) ===\n");

    let config = SyntheticConfig::million_agent_profile(seed);
    eprintln!(
        "Generating synthetic dataset ({} issues, {} simulated agents)...",
        config.issue_count, config.simulated_agent_count
    );

    let dataset = SyntheticDataset::generate(config, &binaries.br.path)
        .expect("Failed to generate synthetic million-agent dataset");

    eprintln!("Running benchmarks...");
    let benchmark = benchmark_synthetic(&dataset, &binaries.br.path);
    print_benchmark(&benchmark);

    let output_dir = PathBuf::from("target/benchmark-results");
    fs::create_dir_all(&output_dir).expect("create output dir");
    let output_path = output_dir.join("synthetic_million_latest.json");
    write_benchmark_json(&[benchmark], &output_path).expect("write results");
    println!("Results written to: {}", output_path.display());
    println!("Manifest written to: {}", dataset.manifest_path.display());
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
    assert_eq!(ScaleTier::Million.issue_count(), 1_000_000);

    assert_eq!(ScaleTier::Small.name(), "small_10k");
    assert_eq!(ScaleTier::Large.name(), "large_100k");
    assert_eq!(ScaleTier::Million.name(), "million_1m");
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

#[test]
fn test_synthetic_config_distribution_builders() {
    let config = SyntheticConfig::ci_profile(7)
        .with_issue_count(64)
        .with_label_distribution(8, 1, 2)
        .with_comment_distribution(0.5, 3)
        .with_agent_distribution(32, 0.25)
        .with_dag_skew(2.0);

    assert_eq!(config.issue_count, 64);
    assert_eq!(config.seed, 7);
    assert_eq!(config.label_pool_size, 8);
    assert_eq!(config.min_labels_per_issue, 1);
    assert_eq!(config.max_labels_per_issue, 2);
    assert!((config.comment_density - 0.5).abs() < f64::EPSILON);
    assert_eq!(config.max_comments_per_issue, 3);
    assert_eq!(config.simulated_agent_count, 32);
    assert!((config.claim_density - 0.25).abs() < f64::EPSILON);
    assert!((config.dag_skew - 2.0).abs() < f64::EPSILON);
}

#[test]
fn synthetic_graph_workloads_cover_skewed_and_wide_profiles() {
    let workloads = synthetic_graph_workloads(96);
    let names = workloads
        .iter()
        .map(|workload| workload.operation)
        .collect::<BTreeSet<_>>();

    assert!(names.contains("graph_hot_hub"));
    assert!(names.contains("dep_tree_hot_leaf"));
    assert!(names.contains("graph_all_components"));

    let dep_tree = workloads
        .iter()
        .find(|workload| workload.operation == "dep_tree_hot_leaf")
        .expect("leaf dependency-tree workload should exist");
    assert!(dep_tree.args.windows(2).any(|window| {
        window.first().is_some_and(|arg| arg == "--max-depth")
            && window.get(1).is_some_and(|arg| arg == "12")
    }));

    let million_workloads = synthetic_graph_workloads(ScaleTier::Million.issue_count());
    assert!(
        million_workloads
            .iter()
            .any(|workload| workload.operation == "graph_hot_hub")
    );
    assert!(
        million_workloads
            .iter()
            .any(|workload| workload.operation == "dep_tree_hot_leaf")
    );
    assert!(
        million_workloads
            .iter()
            .all(|workload| workload.operation != "graph_all_components")
    );
}

#[test]
fn parse_time_max_rss_bytes_reads_gnu_time_output() {
    let stderr = "\
Command being timed: \"br list --json\"\n\
\tUser time (seconds): 0.03\n\
\tMaximum resident set size (kbytes): 12345\n\
\tExit status: 0\n";

    assert_eq!(parse_time_max_rss_bytes(stderr), Some(12_641_280));
}

#[test]
fn sha256_hex_hashes_stdout_for_evidence() {
    assert_eq!(
        sha256_hex(b"br evidence\n"),
        "8dfb2f8fd989532fa7371e6787180e687f541d91995e9a8c27378bc3afbd5406"
    );
}

#[test]
fn normalize_age_minutes_text_redacts_dynamic_age_values() {
    assert_eq!(
        normalize_age_minutes_text(
            "updated_at=1970-01-01T00:00:00Z, age_minutes=29617984, stale_threshold_minutes=120"
        ),
        "updated_at=1970-01-01T00:00:00Z, age_minutes=<normalized>, stale_threshold_minutes=120"
    );
}

#[test]
fn coordination_snapshot_fixture_uses_disjoint_active_and_expired_holders() {
    let temp_dir = TempDir::new().expect("temp dir");
    let config = SyntheticConfig::ci_profile(77).with_agent_distribution(16, 1.0);
    let claimed = (0..16)
        .map(|index| ClaimedIssue {
            id: synthetic_issue_id(index),
            assignee: synthetic_agent_name(config.seed, index),
        })
        .collect::<Vec<_>>();

    let metrics = write_coordination_snapshots(temp_dir.path(), &config, &claimed)
        .expect("write coordination snapshots");
    assert_eq!(metrics.agent_rows, 16);
    assert_eq!(metrics.active_reservation_rows, 16);
    assert_eq!(metrics.expired_reservation_rows, 0);

    let reservations = fs::read_to_string(temp_dir.path().join("coordination-reservations.jsonl"))
        .expect("read reservations");
    assert!(reservations.contains("coordination perf active"));
    assert!(!reservations.contains("coordination perf expired"));
}

#[test]
fn synthetic_ci_profile_benchmarks_graph_projection_workloads() {
    let binaries = discover_binaries().expect("Binary discovery failed");
    let config = SyntheticConfig::ci_profile(101)
        .with_issue_count(96)
        .with_dag_skew(2.0);
    let dataset = SyntheticDataset::generate(config, &binaries.br.path)
        .expect("generate CI synthetic corpus");

    let benchmark = benchmark_synthetic(&dataset, &binaries.br.path);
    let failed_operations = benchmark
        .operations
        .iter()
        .filter(|operation| !operation.success)
        .map(|operation| operation.operation.as_str())
        .collect::<Vec<_>>();
    assert!(
        failed_operations.is_empty(),
        "synthetic CI profile should have no failed operations: {failed_operations:?}"
    );
    assert_eq!(benchmark.config.issue_count, 96);
    assert_eq!(
        benchmark.br_binary_path,
        binaries.br.path.display().to_string()
    );
    assert!(
        benchmark
            .reproduction_command
            .contains("BR_SYNTHETIC_SEED=101")
    );
    let operation_names = benchmark
        .operations
        .iter()
        .map(|operation| operation.operation.as_str())
        .collect::<BTreeSet<_>>();

    for expected in ["graph_hot_hub", "dep_tree_hot_leaf", "graph_all_components"] {
        assert!(operation_names.contains(expected), "missing {expected}");
        let operation = benchmark
            .operations
            .iter()
            .find(|operation| operation.operation == expected)
            .expect("expected operation should be recorded");
        assert!(
            operation.success,
            "{expected} failed: {:?}",
            operation.error.as_deref()
        );
        assert!(
            operation.output_size_bytes > 0,
            "{expected} should emit measurable output"
        );
        assert_eq!(
            operation.stdout_sha256.as_deref().map(str::len),
            Some(64),
            "{expected} should include stdout hash evidence"
        );
    }

    if Path::new("/usr/bin/time").is_file() {
        let missing_rss = benchmark
            .operations
            .iter()
            .filter(|operation| operation.success && operation.peak_rss_bytes.is_none())
            .map(|operation| operation.operation.as_str())
            .collect::<Vec<_>>();
        assert!(
            missing_rss.is_empty(),
            "GNU time should provide child-process RSS for every successful operation; missing: {missing_rss:?}"
        );
    }
}

#[test]
fn synthetic_ci_profile_generates_valid_reproducible_manifest() {
    let binaries = discover_binaries().expect("Binary discovery failed");
    let config = SyntheticConfig::ci_profile(99).with_issue_count(96);

    let first = SyntheticDataset::generate(config.clone(), &binaries.br.path)
        .expect("generate first CI synthetic corpus");
    let second =
        SyntheticDataset::generate(config, &binaries.br.path).expect("generate second CI corpus");

    assert_eq!(first.metrics.issue_count, 96);
    assert_eq!(first.metrics.health.jsonl_issue_count, 96);
    assert_eq!(first.metrics.content_hash, second.metrics.content_hash);
    assert_eq!(
        first.metrics.expected_jsonl_size_bytes,
        first.metrics.jsonl_size_bytes
    );
    assert!(first.metrics.health.jsonl_valid);
    assert!(first.metrics.health.sync_import_ok);
    assert!(first.metrics.health.doctor_ok);
    assert!(first.metrics.health.sync_status_clean);
    assert!(first.metrics.dependency_count > 0);
    assert!(first.metrics.label_assignment_count > 0);
    assert!(first.metrics.comment_count > 0);
    assert!(first.metrics.claim_count > 0);
    assert_eq!(first.metrics.load_strategy, "sync_import");
    assert_eq!(first.metrics.simulated_agent_count, 16);

    let manifest = fs::read_to_string(&first.manifest_path).expect("read corpus manifest");
    let manifest: SyntheticCorpusManifest =
        serde_json::from_str(&manifest).expect("parse corpus manifest");
    assert_eq!(manifest.schema_version, "br.synthetic-corpus.v1");
    assert_eq!(manifest.config.seed, 99);
    assert_eq!(manifest.metrics.content_hash, first.metrics.content_hash);
    assert_eq!(manifest.metrics.load_strategy, "sync_import");
    assert!(
        manifest
            .reproduction_command
            .contains("BR_SYNTHETIC_SEED=99")
    );
}

#[test]
fn direct_sqlite_profile_marks_bypassed_sync_health() {
    let binaries = discover_binaries().expect("Binary discovery failed");
    let config = SyntheticConfig::ci_profile(101).with_issue_count(32);

    let dataset = SyntheticDataset::generate_direct_sqlite(config, &binaries.br.path)
        .expect("generate direct SQLite synthetic corpus");

    assert_eq!(dataset.metrics.issue_count, 32);
    assert_eq!(dataset.metrics.load_strategy, "direct_sqlite_seed");
    assert!(dataset.metrics.health.jsonl_valid);
    assert_eq!(dataset.metrics.health.jsonl_issue_count, 32);
    assert!(!dataset.metrics.health.sync_import_ok);
    assert!(!dataset.metrics.health.doctor_ok);
    assert!(!dataset.metrics.health.sync_status_clean);

    let manifest = fs::read_to_string(&dataset.manifest_path).expect("read corpus manifest");
    let manifest: SyntheticCorpusManifest =
        serde_json::from_str(&manifest).expect("parse corpus manifest");
    assert_eq!(manifest.metrics.load_strategy, "direct_sqlite_seed");
    assert!(!manifest.metrics.health.sync_import_ok);
    assert!(!manifest.metrics.health.doctor_ok);
}

/// Opt-in measurements from real, explicitly pinned release executables. A fresh
/// workspace copy warms the filesystem cache; this does not measure cold cache.
#[cfg(target_os = "linux")]
mod release_abba {
    use super::{
        SyntheticConfig, SyntheticDataset, common, configure_benchmark_environment, sha256_hex,
        write_json_pretty,
    };
    use common::baseline::{
        MatchedRun, MatchedState, compare_matched_runs, summarize_matched_samples,
    };
    use serde::Serialize;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    const CACHE_PROTOCOL: &str = "fresh-copy/warm-filesystem; direct fresh CLI process; copy and validation excluded; ABBA; two warmup blocks; cohort timeout outside measured intervals";
    const RUNNER_PROTOCOL: &str = "identified shared Linux host; one-minute load/logical CPUs <= 0.25; bounded 300s settling before samples with observations retained; compiler inventory retained; A/A calibration required before interpreting A/B";

    #[derive(Serialize)]
    struct Sample {
        side: usize,
        block: usize,
        position: usize,
        warmup: bool,
        elapsed_ms: f64,
        exit_code: i32,
        args: Vec<String>,
        stdout: String,
        stderr: String,
        load_before: String,
        invocations: usize,
    }

    struct Artifact {
        binary: PathBuf,
        metadata: BTreeMap<String, String>,
    }

    fn required(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("required measurement input: {name}"))
    }

    fn count(value: &str, minimum: usize) -> Result<usize, String> {
        value
            .parse::<usize>()
            .ok()
            .filter(|value| *value >= minimum)
            .ok_or_else(|| format!("expected an integer >= {minimum}, received {value:?}"))
    }

    fn workload_budgets(value: &str) -> Result<BTreeMap<String, f64>, String> {
        let budgets: BTreeMap<String, f64> = serde_json::from_str(value)
            .map_err(|error| format!("invalid workload budgets: {error}"))?;
        for (name, budget) in &budgets {
            let known = [1_000, 10_000].into_iter().any(|issue_count| {
                [
                    "version", "ready", "list", "show", "create", "update", "close",
                ]
                .into_iter()
                .any(|command| {
                    ["default-auto-flush", "diagnostic-no-auto-flush"]
                        .into_iter()
                        .any(|flush| name == &format!("{issue_count}-{command}-{flush}"))
                })
            });
            if !known || !budget.is_finite() || *budget < 0.0 {
                return Err(format!(
                    "unknown workload or invalid nonnegative budget: {name}={budget}"
                ));
            }
        }
        Ok(budgets)
    }

    fn text_command(program: &str, args: &[&str]) -> String {
        let output = Command::new(program).args(args).output().expect(program);
        assert!(output.status.success(), "{program}: {output:?}");
        String::from_utf8(output.stdout)
            .expect("UTF-8 metadata")
            .trim()
            .to_string()
    }

    fn isolated_command(binary: &Path, root: &Path, args: &[String]) -> Command {
        // The outer cohort supervisor bounds the whole run. Wrapping each CLI
        // invocation in the installed uutils timeout adds a measured 100ms floor
        // even to /usr/bin/true, concealing short-command latency and regressions.
        let mut command = Command::new(binary);
        command.args(args).current_dir(root);
        configure_benchmark_environment(&mut command, root);
        command
    }

    fn parsed(output: &Output) -> Value {
        assert!(output.status.success(), "CLI failed: {output:?}");
        assert!(!output.stdout.contains(&0x1b), "ANSI in JSON stdout");
        serde_json::from_slice(&output.stdout).expect("entire stdout must be one JSON value")
    }

    fn query(binary: &Path, root: &Path, args: &[&str]) -> Value {
        parsed(
            &isolated_command(
                binary,
                root,
                &args
                    .iter()
                    .map(|arg| (*arg).to_string())
                    .collect::<Vec<_>>(),
            )
            .output()
            .expect("query actual executable"),
        )
    }

    fn runner_load_eligible(load_one: f64, cpus: usize) -> bool {
        cpus > 0 && load_one.is_finite() && load_one >= 0.0 && load_one / cpus as f64 <= 0.25
    }

    fn quiet_runner() -> String {
        assert_eq!(
            required("BR_PERF_QUIET_RUNNER"),
            "1",
            "acknowledge the identified runner and its shared-host measurement protocol"
        );
        let cpus = std::thread::available_parallelism().unwrap().get();
        let started = Instant::now();
        let mut observations = Vec::new();
        let (load, load_one) = loop {
            let load = fs::read_to_string("/proc/loadavg").expect("Linux load metadata");
            let load_one: f64 = load.split_whitespace().next().unwrap().parse().unwrap();
            observations.push(json!({
                "elapsed_ms": started.elapsed().as_secs_f64() * 1_000.0,
                "load_average": load.trim()
            }));
            if runner_load_eligible(load_one, cpus) {
                break (load, load_one);
            }
            assert!(
                started.elapsed() < Duration::from_secs(300),
                "runner did not settle within 300s: {observations:?}"
            );
            std::thread::sleep(
                Duration::from_secs(300)
                    .saturating_sub(started.elapsed())
                    .min(Duration::from_secs(5)),
            );
        };
        let mut compilers = Vec::new();
        for process in fs::read_dir("/proc").expect("process inventory").flatten() {
            if let Ok(name) = fs::read_to_string(process.path().join("comm"))
                && matches!(name.trim(), "rustc" | "cc1" | "cc1plus" | "ld" | "lld")
            {
                compilers.push(json!({
                    "pid": process.file_name().to_string_lossy(),
                    "command": name.trim()
                }));
            }
        }
        json!({
            "load_average": load.trim(), "logical_cpus": cpus,
            "normalized_load_one": load_one / cpus as f64,
            "compiler_processes": compilers, "protocol": RUNNER_PROTOCOL,
            "settling_ms": started.elapsed().as_secs_f64() * 1_000.0,
            "settling_observations": observations
        })
        .to_string()
    }

    fn artifact(side: &str, root: &Path) -> Artifact {
        let binary = PathBuf::from(required(&format!("BR_PERF_{side}_BINARY")))
            .canonicalize()
            .expect("existing executable");
        let binary_sha = sha256_hex(&fs::read(&binary).expect("read executable"));
        assert_eq!(
            binary_sha,
            required(&format!("BR_PERF_{side}_SHA256")),
            "executable differs from explicitly pinned artifact"
        );
        let version = query(&binary, root, &["version", "--json"]);
        assert_eq!(version["build"], "release", "debug is not release evidence");
        let info = query(&binary, root, &["info", "--json"]);
        let cpu = fs::read_to_string("/proc/cpuinfo")
            .expect("CPU metadata")
            .lines()
            .find(|line| line.starts_with("model name"))
            .expect("named CPU")
            .to_string();
        let features = version.get("features").map_or_else(
            || "no-default-features".to_string(),
            |value| serde_json::to_string(value).unwrap(),
        );
        let lock = fs::read(required(&format!("BR_PERF_{side}_LOCKFILE")))
            .expect("lockfile belonging to this artifact");
        let source = required(&format!("BR_PERF_{side}_SOURCE"));
        assert!(
            source.starts_with(version["commit"].as_str().expect("binary source commit")),
            "source identity must begin with the binary's recorded commit"
        );
        let metadata = BTreeMap::from([
            ("source_revision".into(), source),
            ("lockfile_sha256".into(), sha256_hex(&lock)),
            ("binary_sha256".into(), binary_sha),
            ("build_profile".into(), "release".into()),
            ("features".into(), features),
            ("target".into(), version["target"].as_str().unwrap().into()),
            (
                "rust_toolchain".into(),
                version["rust_version"].as_str().unwrap().into(),
            ),
            (
                "engine".into(),
                info["engine"]["version"].as_str().unwrap().into(),
            ),
            ("host".into(), text_command("hostname", &[])),
            ("cpu".into(), cpu),
            ("os".into(), text_command("uname", &["-srm"])),
            (
                "filesystem".into(),
                text_command("stat", &["-f", "-c", "%T", root.to_str().unwrap()]),
            ),
            ("cache_protocol".into(), CACHE_PROTOCOL.into()),
            ("runner_protocol".into(), RUNNER_PROTOCOL.into()),
            (
                "supervisor".into(),
                "none inside measured interval; caller must enforce 1800s cohort deadline".into(),
            ),
            (
                "measurement_harness_sha256".into(),
                sha256_hex(include_bytes!("bench_synthetic_scale.rs")),
            ),
        ]);
        Artifact { binary, metadata }
    }

    fn copy_tree(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).expect("create isolated copy");
        for entry in fs::read_dir(source).expect("read fixture") {
            let entry = entry.expect("fixture entry");
            let kind = entry.file_type().expect("fixture type");
            let target = destination.join(entry.file_name());
            if kind.is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                assert!(kind.is_file(), "fixture must not contain symlinks/devices");
                fs::copy(entry.path(), target).expect("copy fixture file");
            }
        }
    }

    fn arguments(command: &str, id: &str, no_flush: bool) -> Vec<String> {
        let mut args: Vec<String> = match command {
            "version" => vec!["version"],
            "ready" => vec!["ready", "--limit", "20"],
            "list" => vec!["list", "--limit", "20"],
            "show" => vec!["show", id],
            "create" => vec!["create", "ABBA measurement issue", "--type", "task"],
            "update" => vec!["update", id, "--notes", "ABBA measured update"],
            "close" => vec!["close", id, "--reason", "ABBA measured completion"],
            _ => panic!("unsupported measurement command: {command}"),
        }
        .into_iter()
        .map(str::to_string)
        .collect();
        args.push("--json".into());
        if no_flush {
            args.push("--no-auto-flush".into());
        }
        args
    }

    fn verify_mutation(binary: &Path, root: &Path, command: &str, id: &str, value: &Value) {
        if !matches!(command, "create" | "update" | "close") {
            return;
        }
        let observed_id = if command == "create" {
            value["id"].as_str().expect("created issue ID")
        } else {
            id
        };
        let shown = query(binary, root, &["show", observed_id, "--json"]);
        let issue = &shown[0];
        assert_eq!(issue["id"], observed_id);
        match command {
            "create" => assert_eq!(issue["title"], "ABBA measurement issue"),
            "update" => assert_eq!(issue["notes"], "ABBA measured update"),
            "close" => assert_eq!(issue["status"], "closed"),
            _ => unreachable!("mutation only"),
        }
    }

    fn stripped_size(artifact: &Artifact) -> Value {
        let target = &artifact.metadata["target"];
        if !matches!(
            target.as_str(),
            "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu"
        ) {
            return json!({"target": target, "state": "unsupported_target", "gate_exit": 2});
        }
        let sections = text_command("readelf", &["-S", artifact.binary.to_str().unwrap()]);
        assert!(
            !sections.contains(".symtab"),
            "release artifact must be stripped"
        );
        let compressed = Command::new("gzip")
            .args(["-n", "-c"])
            .arg(&artifact.binary)
            .output()
            .expect("measure deterministic gzip stream");
        assert!(compressed.status.success(), "gzip: {compressed:?}");
        json!({
            "target": target,
            "binary_sha256": artifact.metadata["binary_sha256"],
            "stripped_bytes": fs::metadata(&artifact.binary).unwrap().len(),
            "gzip_bytes": compressed.stdout.len(),
            "compression": "gzip -n -c (binary, not release archive)",
            "state": "measured_without_accepted_size_budget", "gate_exit": 2
        })
    }

    #[test]
    fn release_measurement_controls_reject_vacuous_counts() {
        assert_eq!(count("10", 10), Ok(10));
        for invalid in ["0", "9", "-1", "", "garbage"] {
            assert!(count(invalid, 10).is_err(), "accepted {invalid:?}");
        }
        assert!(
            arguments("update", "syn-1", false)
                .iter()
                .all(|arg| arg != "--no-auto-flush")
        );
        assert!(
            arguments("update", "syn-1", true)
                .iter()
                .any(|arg| arg == "--no-auto-flush")
        );
        assert!(runner_load_eligible(4.0, 16));
        assert!(!runner_load_eligible(4.01, 16));
        assert!(!runner_load_eligible(0.0, 0));
        assert!(!runner_load_eligible(-1.0, 16));
        assert!(!runner_load_eligible(f64::NAN, 16));
        assert!(!runner_load_eligible(f64::INFINITY, 16));
        assert_eq!(
            workload_budgets(r#"{"1000-ready-default-auto-flush":12.5}"#).unwrap()["1000-ready-default-auto-flush"].to_bits(),
            12.5_f64.to_bits()
        );
        assert!(workload_budgets("{}").unwrap().is_empty());
        for invalid in [
            r#"{"1000-typo-default-auto-flush":10}"#,
            r#"{"1000-ready-default-auto-flush":-1}"#,
            r#"{"1000-ready-default-auto-flush":"10"}"#,
            r#"{"1000-ready-default-auto-flush":null}"#,
            "[]",
            "invalid",
        ] {
            assert!(workload_budgets(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn release_measurement_invokes_cli_without_interposed_supervisor() {
        let workspace = TempDir::new_in(common::cli::isolated_temp_root()).unwrap();
        let output = isolated_command(
            Path::new("/bin/sh"),
            workspace.path(),
            &["-c".into(), "printf '%s' \"$PPID\"".into()],
        )
        .output()
        .expect("observe actual child process parent");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            std::process::id().to_string(),
            "the timed program must be a direct child; an interposed supervisor changes short-command measurements"
        );
    }

    #[test]
    #[ignore = "requires pinned release artifacts and an identified low-contention Linux runner"]
    fn release_cli_abba_1k_10k() {
        let blocks = count(
            &std::env::var("BR_PERF_ABBA_BLOCKS").unwrap_or_else(|_| "10".into()),
            10,
        )
        .expect("at least twenty retained observations per artifact");
        let repeats = count(
            &std::env::var("BR_PERF_NEGATIVE_EXTRA_WORK").unwrap_or_else(|_| "1".into()),
            1,
        )
        .expect("positive real invocation count");
        let budget = std::env::var("BR_PERF_BUDGET_PCT")
            .ok()
            .map(|value| value.parse::<f64>().expect("numeric budget"));
        let budgets = workload_budgets(
            &std::env::var("BR_PERF_BUDGETS_JSON").unwrap_or_else(|_| "{}".into()),
        )
        .expect("explicit per-workload relative budgets");
        assert!(
            budget.is_none() || budgets.is_empty(),
            "diagnostic global and per-workload budgets must not be mixed"
        );
        let output_dir = PathBuf::from(required("BR_PERF_OUTPUT"));
        fs::create_dir_all(&output_dir).expect("retain raw measurement outputs");
        let initial_load = quiet_runner();
        let mut gate_exits = Vec::new();
        for issue_count in [1_000, 10_000] {
            let config = SyntheticConfig {
                dependency_density: 0.25,
                comment_density: 0.0,
                claim_density: 0.0,
                simulated_agent_count: 8,
                ..SyntheticConfig::ci_profile(42)
                    .with_issue_count(issue_count)
                    .with_label_distribution(16, 0, 2)
            };
            let paths = [
                PathBuf::from(required("BR_PERF_BASELINE_BINARY")),
                PathBuf::from(required("BR_PERF_CANDIDATE_BINARY")),
            ];
            let datasets = paths.each_ref().map(|path| {
                SyntheticDataset::generate(config.clone(), path)
                    .expect("real CLI import and health checks")
            });
            for dataset in &datasets {
                assert_eq!(dataset.metrics.issue_count, issue_count);
                assert!(dataset.metrics.health.jsonl_valid);
                assert!(dataset.metrics.health.sync_import_ok);
                assert!(dataset.metrics.health.doctor_ok);
                assert!(dataset.metrics.health.sync_status_clean);
            }
            assert_eq!(
                datasets[0].metrics.content_hash,
                datasets[1].metrics.content_hash
            );
            let artifacts = [
                artifact("BASELINE", &datasets[0].root),
                artifact("CANDIDATE", &datasets[1].root),
            ];
            if repeats > 1 {
                assert_eq!(
                    artifacts[0].metadata["binary_sha256"], artifacts[1].metadata["binary_sha256"],
                    "the planted slowdown is an A/A control using one artifact"
                );
                assert!(
                    budget.is_some(),
                    "a planted regression needs an explicit budget"
                );
            }
            write_json_pretty(
                &output_dir.join(format!("{issue_count}-artifact-identities.json")),
                &json!({
                    "baseline": artifacts[0].metadata,
                    "candidate": artifacts[1].metadata,
                    "corpus": datasets[0].metrics,
                    "initial_load": initial_load
                }),
            )
            .unwrap();
            let ready = query(
                &artifacts[0].binary,
                &datasets[0].root,
                &["ready", "--limit", "1", "--json"],
            );
            let id = ready[0]["id"]
                .as_str()
                .expect("representative unblocked issue");
            for no_flush in [false, true] {
                for command in [
                    "version", "ready", "list", "show", "create", "update", "close",
                ] {
                    let flush_mode = if no_flush {
                        "diagnostic-no-auto-flush"
                    } else {
                        "default-auto-flush"
                    };
                    let name = format!("{issue_count}-{command}-{flush_mode}");
                    let expected_read = if matches!(command, "ready" | "list" | "show") {
                        let output = isolated_command(
                            &artifacts[0].binary,
                            &datasets[0].root,
                            &arguments(command, id, no_flush),
                        )
                        .output()
                        .unwrap();
                        let expected = parsed(&output);
                        let issues = expected
                            .get("issues")
                            .unwrap_or(&expected)
                            .as_array()
                            .expect("issue array");
                        assert_eq!(issues.len(), if command == "show" { 1 } else { 20 });
                        assert!(issues.iter().all(|issue| issue["id"].as_str().is_some()));
                        Some(expected)
                    } else {
                        None
                    };
                    let mut samples = Vec::new();
                    let mut runs = artifacts.each_ref().map(|artifact| {
                        let mut metadata = artifact.metadata.clone();
                        metadata.extend([
                            ("command".into(), command.into()),
                            ("issue_count".into(), issue_count.to_string()),
                            (
                                "dataset_sha256".into(),
                                datasets[0].metrics.content_hash.clone(),
                            ),
                            ("flush_mode".into(), flush_mode.into()),
                        ]);
                        MatchedRun {
                            metadata,
                            samples_ms: Vec::new(),
                            exit_codes: Vec::new(),
                        }
                    });
                    for block in 0..blocks + 2 {
                        for (position, side) in [0, 1, 1, 0].into_iter().enumerate() {
                            let load_before = quiet_runner();
                            let workspace =
                                TempDir::new_in(common::cli::isolated_temp_root()).unwrap();
                            copy_tree(&datasets[side].beads_dir, &workspace.path().join(".beads"));
                            let args = arguments(command, id, no_flush);
                            let invocations = if side == 1 && command == "ready" {
                                repeats
                            } else {
                                1
                            };
                            let mut commands: Vec<_> = (0..invocations)
                                .map(|_| {
                                    isolated_command(
                                        &artifacts[side].binary,
                                        workspace.path(),
                                        &args,
                                    )
                                })
                                .collect();
                            let mut outputs = Vec::with_capacity(invocations);
                            let started = Instant::now();
                            for invocation in &mut commands {
                                outputs.push(invocation.output().expect("measured CLI invocation"));
                            }
                            let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
                            let exit_code = outputs
                                .iter()
                                .find(|output| !output.status.success())
                                .unwrap_or_else(|| outputs.last().unwrap())
                                .status
                                .code()
                                .unwrap_or(-1);
                            samples.push(Sample {
                                side,
                                block,
                                position,
                                warmup: block < 2,
                                elapsed_ms,
                                exit_code,
                                args,
                                stdout: outputs
                                    .iter()
                                    .map(|output| String::from_utf8_lossy(&output.stdout))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                stderr: outputs
                                    .iter()
                                    .map(|output| String::from_utf8_lossy(&output.stderr))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                load_before,
                                invocations,
                            });
                            write_json_pretty(
                                &output_dir.join(format!("{name}-raw.json")),
                                &samples,
                            )
                            .unwrap();
                            if exit_code != 0 {
                                write_json_pretty(
                                    &output_dir.join("run.json"),
                                    &json!({
                                        "state": "inconclusive", "gate_exit": 2,
                                        "failed_workload": name, "side": side,
                                        "block": block, "exit_code": exit_code,
                                        "reason": "command failed; raw stdout/stderr retained"
                                    }),
                                )
                                .unwrap();
                            }
                            for output in &outputs {
                                let value = parsed(output);
                                if let Some(expected) = &expected_read {
                                    assert_eq!(
                                        &value, expected,
                                        "sampled read differs from the populated baseline fixture"
                                    );
                                }
                                verify_mutation(
                                    &artifacts[side].binary,
                                    workspace.path(),
                                    command,
                                    id,
                                    &value,
                                );
                            }
                            if block >= 2 {
                                runs[side].samples_ms.push(elapsed_ms);
                                runs[side].exit_codes.push(exit_code);
                            }
                        }
                    }
                    for (side, run) in runs.iter().enumerate() {
                        run.validate()
                            .expect("real observations and complete metadata");
                        write_json_pretty(&output_dir.join(format!("{name}-{side}.json")), run)
                            .unwrap();
                    }
                    let comparison = compare_matched_runs(
                        Some(&runs[0]),
                        &runs[1],
                        budgets.get(&name).copied().or(budget).unwrap_or(f64::NAN),
                    );
                    gate_exits.push(comparison.exit_code());
                    write_json_pretty(
                        &output_dir.join(format!("{name}-summary.json")),
                        &json!({
                            "baseline": summarize_matched_samples(&runs[0].samples_ms).unwrap(),
                            "candidate": summarize_matched_samples(&runs[1].samples_ms).unwrap(),
                            "comparison": comparison, "gate_exit": comparison.exit_code(),
                            "negative_control_ready_invocations": repeats,
                            "budget_origin": if budgets.contains_key(&name) {
                                "operator-supplied per-workload budget"
                            } else if budget.is_some() {
                                "global diagnostic control; not an accepted SLO"
                            } else {
                                "missing; comparison remains inconclusive"
                            },
                            "corpus": datasets[0].metrics,
                            "startup_objectives_ms": {"warm_version": 50, "cold_version": 100},
                            "cold_cache_state": "UNPROVEN; no cache eviction performed"
                        }),
                    )
                    .unwrap();
                    if repeats > 1 && command == "ready" {
                        assert_eq!(
                            comparison.state,
                            MatchedState::Regression,
                            "planted extra CLI work must produce a regression, not unknown"
                        );
                    }
                }
            }
            write_json_pretty(
                &output_dir.join(format!("{issue_count}-artifact-sizes.json")),
                &artifacts.each_ref().map(stripped_size),
            )
            .unwrap();
        }
        write_json_pretty(&output_dir.join("run.json"), &json!({
            "state": "measurements_completed", "initial_load": initial_load,
            "cache_protocol": CACHE_PROTOCOL, "latency_gate_exits": gate_exits,
            "release_budget_gate_exit": 2,
            "release_budget_state": "inconclusive: target size budgets not calibrated",
            "workload_budgets": budgets,
            "budget_status": if budget.is_some() { "operator-supplied diagnostic budget; not an accepted SLO" } else if budgets.is_empty() { "inconclusive: no calibrated budget supplied" } else { "operator-supplied per-workload budgets; missing entries remain inconclusive" }
        })).unwrap();
        if budget.is_some() && repeats == 1 {
            assert!(
                gate_exits.iter().all(|code| *code == 0),
                "regression or inconclusive comparison; inspect retained receipts"
            );
        }
    }
}
