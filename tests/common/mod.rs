#![allow(dead_code)]

use beads_rust::storage::SqliteStorage;
use std::sync::Once;
use tempfile::TempDir;

pub mod assertions;
pub mod cli;
pub mod fixtures;
pub mod scenarios;

static INIT: Once = Once::new();

pub fn init_test_logging() {
    INIT.call_once(|| {
        beads_rust::logging::init_test_logging();
    });
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
