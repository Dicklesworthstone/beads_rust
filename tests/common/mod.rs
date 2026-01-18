#![allow(dead_code)]

use beads_rust::storage::SqliteStorage;
use std::sync::Once;
use std::time::Instant;
use tempfile::TempDir;
use tracing::info;

pub mod assertions;
pub mod cli;
pub mod dataset_registry;
pub mod fixtures;
pub mod harness;
pub mod scenarios;

pub use dataset_registry::{DatasetMetadata, DatasetRegistry, IsolatedDataset, KnownDataset};

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
