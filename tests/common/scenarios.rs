#![allow(dead_code)]

use super::*;
use beads_rust::model::Issue;

pub fn scenario_simple_issues(count: usize) -> (SqliteStorage, Vec<Issue>) {
    let mut storage = test_db();
    let mut issues = Vec::new();
    for i in 0..count {
        let issue = fixtures::issue(&format!("Issue {i}"));
        storage.create_issue(&issue, "tester").unwrap();
        issues.push(issue);
    }
    (storage, issues)
}
