//! Atomic claim guard tests — verifies TOCTOU-safe claiming via IMMEDIATE transactions.

use beads_rust::model::{Priority, Status};
use beads_rust::storage::{IssueUpdate, SqliteStorage};
use chrono::{TimeZone, Utc};
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

/// Helper to create a minimal issue for testing.
fn seed_issue(storage: &mut SqliteStorage, id: &str, assignee: Option<&str>) {
    let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let issue = beads_rust::model::Issue {
        id: id.to_string(),
        title: format!("Test issue {id}"),
        status: Status::Open,
        priority: Priority(2),
        issue_type: beads_rust::model::IssueType::Task,
        created_at: t,
        updated_at: t,
        assignee: assignee.map(str::to_string),
        content_hash: None,
        description: None,
        design: None,
        acceptance_criteria: None,
        notes: None,
        owner: None,
        estimated_minutes: None,
        created_by: None,
        closed_at: None,
        close_reason: None,
        closed_by_session: None,
        due_at: None,
        defer_until: None,
        external_ref: None,
        source_system: None,
        source_repo: None,
        source_repo_path: None,
        agent_context: None,
        deleted_at: None,
        deleted_by: None,
        delete_reason: None,
        original_type: None,
        compaction_level: None,
        compacted_at: None,
        compacted_at_commit: None,
        original_size: None,
        sender: None,
        ephemeral: false,
        pinned: false,
        is_template: false,
        labels: vec![],
        dependencies: vec![],
        comments: vec![],
    };
    storage.create_issue(&issue, "seed").unwrap();
}

#[test]
fn test_claim_unassigned_succeeds() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    seed_issue(&mut storage, "test-1", None);

    let update = IssueUpdate {
        status: Some(Status::InProgress),
        assignee: Some(Some("alice".to_string())),
        expect_unassigned: true,
        claim_actor: Some("alice".to_string()),
        ..IssueUpdate::default()
    };

    let result = storage.update_issue("test-1", &update, "alice");
    assert!(result.is_ok());
    let issue = result.unwrap();
    assert_eq!(issue.assignee.as_deref(), Some("alice"));
    assert_eq!(issue.status, Status::InProgress);
}

#[test]
fn test_claim_already_assigned_different_actor_fails() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    seed_issue(&mut storage, "test-2", Some("bob"));

    let update = IssueUpdate {
        status: Some(Status::InProgress),
        assignee: Some(Some("alice".to_string())),
        expect_unassigned: true,
        claim_actor: Some("alice".to_string()),
        ..IssueUpdate::default()
    };

    let result = storage.update_issue("test-2", &update, "alice");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("already assigned to bob"), "Error was: {err}");
}

#[test]
fn test_claim_same_actor_idempotent() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    seed_issue(&mut storage, "test-3", Some("alice"));

    let update = IssueUpdate {
        status: Some(Status::InProgress),
        assignee: Some(Some("alice".to_string())),
        expect_unassigned: true,
        claim_actor: Some("alice".to_string()),
        ..IssueUpdate::default()
    };

    let result = storage.update_issue("test-3", &update, "alice");
    assert!(result.is_ok(), "Same-actor re-claim should be idempotent");
}

#[test]
fn test_claim_exclusive_rejects_same_actor() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    seed_issue(&mut storage, "test-4", Some("alice"));

    let update = IssueUpdate {
        status: Some(Status::InProgress),
        assignee: Some(Some("alice".to_string())),
        expect_unassigned: true,
        claim_exclusive: true,
        claim_actor: Some("alice".to_string()),
        ..IssueUpdate::default()
    };

    let result = storage.update_issue("test-4", &update, "alice");
    assert!(
        result.is_err(),
        "Exclusive mode should reject same-actor re-claim"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("already assigned to alice"),
        "Error was: {err}"
    );
}

#[test]
fn test_claim_whitespace_assignee_treated_as_unassigned() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    seed_issue(&mut storage, "test-5", Some("   "));

    let update = IssueUpdate {
        status: Some(Status::InProgress),
        assignee: Some(Some("alice".to_string())),
        expect_unassigned: true,
        claim_actor: Some("alice".to_string()),
        ..IssueUpdate::default()
    };

    let result = storage.update_issue("test-5", &update, "alice");
    assert!(
        result.is_ok(),
        "Whitespace-only assignee should be treated as unassigned"
    );
}

#[test]
fn test_claim_empty_string_assignee_treated_as_unassigned() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    seed_issue(&mut storage, "test-6", Some(""));

    let update = IssueUpdate {
        status: Some(Status::InProgress),
        assignee: Some(Some("alice".to_string())),
        expect_unassigned: true,
        claim_actor: Some("alice".to_string()),
        ..IssueUpdate::default()
    };

    let result = storage.update_issue("test-6", &update, "alice");
    assert!(
        result.is_ok(),
        "Empty-string assignee should be treated as unassigned"
    );
}

#[test]
#[allow(clippy::needless_collect)]
fn test_concurrent_claim_exactly_one_wins() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("beads.db");

    // Seed the issue
    {
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        seed_issue(&mut storage, "race-1", None);
    }

    let barrier = Arc::new(Barrier::new(2));
    let db_lock = Arc::new(std::sync::Mutex::new(()));
    let path = db_path.to_string_lossy().into_owned();

    let handles: Vec<_> = ["alice", "bob"]
        .iter()
        .map(|actor| {
            let barrier = Arc::clone(&barrier);
            let db_lock = Arc::clone(&db_lock);
            let path = path.clone();
            let actor = actor.to_string();
            thread::spawn(move || {
                barrier.wait();
                let _guard = db_lock.lock().unwrap();
                let mut storage = SqliteStorage::open(Path::new(&path)).unwrap();

                let update = IssueUpdate {
                    status: Some(Status::InProgress),
                    assignee: Some(Some(actor.clone())),
                    expect_unassigned: true,
                    claim_actor: Some(actor.clone()),
                    ..IssueUpdate::default()
                };

                storage.update_issue("race-1", &update, &actor)
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let successes = results.iter().filter(|r| r.is_ok()).count();
    let failures = results.iter().filter(|r| r.is_err()).count();
    for err in results.iter().filter(|r| r.is_err()) {
        println!("Failure: {:?}", err);
    }

    assert_eq!(successes, 1, "Exactly one agent should win the race");
    assert_eq!(failures, 1, "Exactly one agent should lose the race");
}

#[test]
fn test_concurrent_claim_different_issues_both_succeed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("beads.db");

    {
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        seed_issue(&mut storage, "diff-1", None);
        seed_issue(&mut storage, "diff-2", None);
    }

    let barrier = Arc::new(Barrier::new(2));
    let db_lock = Arc::new(std::sync::Mutex::new(()));
    let path = db_path.to_string_lossy().into_owned();

    let h1 = {
        let barrier = Arc::clone(&barrier);
        let db_lock = Arc::clone(&db_lock);
        let path = path.clone();
        thread::spawn(move || {
            barrier.wait();
            let _guard = db_lock.lock().unwrap();
            let mut storage = SqliteStorage::open(Path::new(&path)).unwrap();
            let update = IssueUpdate {
                status: Some(Status::InProgress),
                assignee: Some(Some("alice".to_string())),
                expect_unassigned: true,
                claim_actor: Some("alice".to_string()),
                ..IssueUpdate::default()
            };
            storage.update_issue("diff-1", &update, "alice")
        })
    };

    let h2 = {
        let barrier = Arc::clone(&barrier);
        let db_lock = Arc::clone(&db_lock);
        thread::spawn(move || {
            barrier.wait();
            let _guard = db_lock.lock().unwrap();
            let mut storage = SqliteStorage::open(Path::new(&path)).unwrap();
            let update = IssueUpdate {
                status: Some(Status::InProgress),
                assignee: Some(Some("bob".to_string())),
                expect_unassigned: true,
                claim_actor: Some("bob".to_string()),
                ..IssueUpdate::default()
            };
            storage.update_issue("diff-2", &update, "bob")
        })
    };

    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();
    if let Err(ref e) = r1 {
        println!("r1 failed: {e:?}");
    }
    if let Err(ref e) = r2 {
        println!("r2 failed: {e:?}");
    }
    assert!(r1.is_ok(), "alice should claim diff-1");
    assert!(r2.is_ok(), "bob should claim diff-2");
}

#[test]
fn test_non_claim_update_skips_guard() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    seed_issue(&mut storage, "test-nc", Some("bob"));

    // Regular update (not a claim) should succeed even though assigned to someone else
    let update = IssueUpdate {
        title: Some("New title".to_string()),
        expect_unassigned: false,
        ..IssueUpdate::default()
    };

    let result = storage.update_issue("test-nc", &update, "alice");
    assert!(result.is_ok(), "Non-claim update should not check assignee");
}
