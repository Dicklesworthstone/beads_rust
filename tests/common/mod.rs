#![allow(dead_code, unused_imports)]

use beads_rust::storage::SqliteStorage;
use std::sync::Once;
use std::time::Instant;
use tempfile::TempDir;
use tracing::info;

pub mod artifact_validator;
pub mod assertions;
pub mod baseline;
pub mod binary_discovery;
pub mod cli;
pub mod dataset_registry;
pub mod fixtures;
pub mod harness;
pub mod json_baseline;
pub mod report_indexer;
pub mod scenarios;

pub use artifact_validator::ArtifactValidator;
pub use baseline::{
    BaselineStore, RegressionConfig, RegressionResult, RegressionSummary, RegressionStatus,
    should_update_baseline, update_baselines_from_results,
};
pub use binary_discovery::{BinaryVersion, DiscoveredBinaries, discover_binaries};
pub use dataset_registry::{
    DatasetIntegrityGuard, DatasetMetadata, DatasetOverride, DatasetProvenance, DatasetRegistry,
    IntegrityCheckResult, IsolatedDataset, KnownDataset, isolated_from_override,
    run_with_integrity,
};
pub use harness::{ParallelismMode, ResourceGuardrails, RunnerPolicy};
pub use report_indexer::{
    ArtifactIndexer, CommandMetric, FullReport, IndexerConfig, IndexerError, SnapshotMetric,
    SuiteReport, TestReport, generate_html_report, generate_markdown_report, write_reports,
};
pub use scenarios::{
    CompareMode, ExecutionMode, Invariants, NormalizationRules, Scenario, ScenarioCommand,
    ScenarioFilter, ScenarioResult, ScenarioRunner, ScenarioSetup, TagMatchMode,
};

static INIT: Once = Once::new();

pub fn init_test_logging() {
    INIT.call_once(|| {
        beads_rust::logging::init_test_logging();
    });
}

pub struct TestLogGuard {
    name: String,
    start: Instant,
}

impl TestLogGuard {
    fn new(name: &str) -> Self {
        init_test_logging();
        info!("{name}: starting");
        Self {
            name: name.to_string(),
            start: Instant::now(),
        }
    }
}

impl Drop for TestLogGuard {
    fn drop(&mut self) {
        info!(
            "{}: assertions passed (elapsed {:?})",
            self.name,
            self.start.elapsed()
        );
    }
}

pub fn test_log(name: &str) -> TestLogGuard {
    TestLogGuard::new(name)
}

pub fn test_db() -> SqliteStorage {
    init_test_logging();
    SqliteStorage::open_memory().expect("Failed to create test database")
}

pub fn test_db_with_dir() -> (SqliteStorage, TempDir) {
    init_test_logging();
    let dir = TempDir::new().expect("Failed to create temp dir");
    let db_path = dir.path().join(".beads").join("beads.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let storage = SqliteStorage::open(&db_path).expect("Failed to create test database");
    (storage, dir)
}
