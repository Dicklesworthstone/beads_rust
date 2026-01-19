#![allow(dead_code)]

use beads_rust::model::Status;
use beads_rust::storage::SqliteStorage;
use tracing::info;

pub fn assert_issue_exists(storage: &SqliteStorage, id: &str) {
    info!("Asserting issue exists: {}", id);
    let issue = storage
        .get_issue(id)
        .unwrap_or_else(|err| panic!("get_issue failed for {id}: {err}"));
    assert!(issue.is_some(), "expected issue {id} to exist");
}

pub fn assert_status(storage: &SqliteStorage, id: &str, expected: &Status) {
    info!("Asserting status of {} is {}", id, expected);
    let issue = storage
        .get_issue(id)
        .unwrap_or_else(|err| panic!("get_issue failed for {id}: {err}"))
        .unwrap_or_else(|| panic!("expected issue {id} to exist"));
    assert_eq!(
        &issue.status, expected,
        "expected status of {} to be {}, got {}",
        id, expected, issue.status
    );
}
