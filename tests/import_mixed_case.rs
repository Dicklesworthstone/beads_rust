//! Regression tests for mixed-case Status/IssueType JSONL import.
//!
//! Verifies that import_from_jsonl correctly normalizes case-variant
//! strings ("In_Progress", "BUG", "INPROGRESS") to canonical enum
//! values, and that content hashes remain stable after round-trip.

use beads_rust::model::{IssueType, Status};
use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{ExportConfig, ImportConfig, export_to_jsonl, import_from_jsonl};
use beads_rust::util::ContentHashable;
use std::fs;
use tempfile::TempDir;

fn setup() -> (
    TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    SqliteStorage,
) {
    let temp = TempDir::new().expect("temp dir");
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let db_path = beads_dir.join("beads.db");
    let storage = SqliteStorage::open(&db_path).unwrap();
    (temp, beads_dir, jsonl_path, storage)
}

fn import_config(beads_dir: &std::path::Path) -> ImportConfig {
    ImportConfig {
        skip_prefix_validation: true,
        clear_duplicate_external_refs: true,
        beads_dir: Some(beads_dir.to_path_buf()),
        show_progress: false,
        ..ImportConfig::default()
    }
}

fn export_config(beads_dir: &std::path::Path) -> ExportConfig {
    ExportConfig {
        force: true,
        is_default_path: true,
        beads_dir: Some(beads_dir.to_path_buf()),
        show_progress: false,
        ..ExportConfig::default()
    }
}

fn make_jsonl_issue(id: &str, title: &str, status: &str, issue_type: &str) -> String {
    format!(
        r#"{{"id":"{id}","title":"{title}","status":"{status}","priority":2,"issue_type":"{issue_type}","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}"#
    )
}

#[test]
fn import_mixed_case_status_normalizes() {
    let (_temp, beads_dir, jsonl_path, mut storage) = setup();

    let jsonl = [
        make_jsonl_issue("bd-mc1", "Open variant", "OPEN", "task"),
        make_jsonl_issue("bd-mc2", "InProgress variant", "In_Progress", "task"),
        make_jsonl_issue("bd-mc3", "InProgress alias", "INPROGRESS", "task"),
        make_jsonl_issue("bd-mc4", "Draft variant", "Draft", "task"),
        make_jsonl_issue("bd-mc5", "Closed variant", "CLOSED", "task"),
        make_jsonl_issue("bd-mc6", "Blocked variant", "Blocked", "task"),
    ]
    .join("\n");
    fs::write(&jsonl_path, &jsonl).unwrap();

    let result = import_from_jsonl(
        &mut storage,
        &jsonl_path,
        &import_config(&beads_dir),
        Some("bd"),
    );
    assert!(result.is_ok(), "import failed: {:?}", result.err());

    let issues = storage.get_all_issues_for_export().unwrap();
    assert_eq!(issues.len(), 6, "should have imported 6 issues");

    let find = |id: &str| issues.iter().find(|i| i.id == id).unwrap();

    assert_eq!(find("bd-mc1").status, Status::Open);
    assert_eq!(find("bd-mc2").status, Status::InProgress);
    assert_eq!(find("bd-mc3").status, Status::InProgress);
    assert_eq!(find("bd-mc4").status, Status::Draft);
    assert_eq!(find("bd-mc5").status, Status::Closed);
    assert_eq!(find("bd-mc6").status, Status::Blocked);
}

#[test]
fn import_mixed_case_issue_type_normalizes() {
    let (_temp, beads_dir, jsonl_path, mut storage) = setup();

    let jsonl = [
        make_jsonl_issue("bd-it1", "Bug type", "open", "Bug"),
        make_jsonl_issue("bd-it2", "Feature type", "open", "FEATURE"),
        make_jsonl_issue("bd-it3", "Epic type", "open", "Epic"),
        make_jsonl_issue("bd-it4", "Chore type", "open", "CHORE"),
        make_jsonl_issue("bd-it5", "Docs type", "open", "Docs"),
        make_jsonl_issue("bd-it6", "Question type", "open", "QUESTION"),
    ]
    .join("\n");
    fs::write(&jsonl_path, &jsonl).unwrap();

    let result = import_from_jsonl(
        &mut storage,
        &jsonl_path,
        &import_config(&beads_dir),
        Some("bd"),
    );
    assert!(result.is_ok(), "import failed: {:?}", result.err());

    let issues = storage.get_all_issues_for_export().unwrap();
    assert_eq!(issues.len(), 6);

    let find = |id: &str| issues.iter().find(|i| i.id == id).unwrap();

    assert_eq!(find("bd-it1").issue_type, IssueType::Bug);
    assert_eq!(find("bd-it2").issue_type, IssueType::Feature);
    assert_eq!(find("bd-it3").issue_type, IssueType::Epic);
    assert_eq!(find("bd-it4").issue_type, IssueType::Chore);
    assert_eq!(find("bd-it5").issue_type, IssueType::Docs);
    assert_eq!(find("bd-it6").issue_type, IssueType::Question);
}

#[test]
fn import_export_roundtrip_normalizes_case() {
    let (_temp, beads_dir, jsonl_path, mut storage) = setup();

    let jsonl = [
        make_jsonl_issue("bd-rt1", "Round trip", "In_Progress", "Bug"),
        make_jsonl_issue("bd-rt2", "Another one", "DRAFT", "FEATURE"),
    ]
    .join("\n");
    fs::write(&jsonl_path, &jsonl).unwrap();

    import_from_jsonl(
        &mut storage,
        &jsonl_path,
        &import_config(&beads_dir),
        Some("bd"),
    )
    .expect("import should succeed");

    let export_path = beads_dir.join("exported.jsonl");
    export_to_jsonl(&storage, &export_path, &export_config(&beads_dir))
        .expect("export should succeed");

    let exported = fs::read_to_string(&export_path).unwrap();

    for line in exported.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        let status = v["status"].as_str().unwrap();
        let issue_type = v["issue_type"].as_str().unwrap();

        assert_eq!(
            status,
            status.to_lowercase(),
            "exported status should be lowercase canonical: got {status}"
        );
        assert_eq!(
            issue_type,
            issue_type.to_lowercase(),
            "exported issue_type should be lowercase canonical: got {issue_type}"
        );
    }
}

#[test]
fn import_mixed_case_content_hash_matches_canonical() {
    let (_temp, beads_dir, jsonl_path, mut storage) = setup();

    let jsonl_mixed = make_jsonl_issue("bd-hash1", "Hash stability", "In_Progress", "Bug");
    fs::write(&jsonl_path, &jsonl_mixed).unwrap();
    import_from_jsonl(
        &mut storage,
        &jsonl_path,
        &import_config(&beads_dir),
        Some("bd"),
    )
    .unwrap();
    let mixed_issues = storage.get_all_issues_for_export().unwrap();
    let mixed_hash = mixed_issues[0].content_hash();

    let (_temp2, beads_dir2, jsonl_path2, mut storage2) = setup();
    let jsonl_canonical = make_jsonl_issue("bd-hash1", "Hash stability", "in_progress", "bug");
    fs::write(&jsonl_path2, &jsonl_canonical).unwrap();
    import_from_jsonl(
        &mut storage2,
        &jsonl_path2,
        &import_config(&beads_dir2),
        Some("bd"),
    )
    .unwrap();
    let canonical_issues = storage2.get_all_issues_for_export().unwrap();
    let canonical_hash = canonical_issues[0].content_hash();

    assert_eq!(
        mixed_hash, canonical_hash,
        "content hash must be identical regardless of input case"
    );
}

#[test]
fn import_custom_status_normalizes_case_through_roundtrip() {
    let (_temp, beads_dir, jsonl_path, mut storage) = setup();

    let jsonl = make_jsonl_issue("bd-cust1", "Custom status", "QA_Review", "task");
    fs::write(&jsonl_path, &jsonl).unwrap();

    import_from_jsonl(
        &mut storage,
        &jsonl_path,
        &import_config(&beads_dir),
        Some("bd"),
    )
    .unwrap();

    let issues = storage.get_all_issues_for_export().unwrap();
    assert_eq!(issues.len(), 1);
    match &issues[0].status {
        Status::Custom(val) => {
            assert_eq!(
                val, "qa_review",
                "custom status normalizes to lowercase through DB round-trip"
            );
        }
        other => panic!("Expected Custom status, got {:?}", other),
    }
}

#[test]
fn import_custom_issue_type_normalizes_case_through_roundtrip() {
    let (_temp, beads_dir, jsonl_path, mut storage) = setup();

    let jsonl = make_jsonl_issue("bd-cust2", "Custom type", "open", "Security_Audit");
    fs::write(&jsonl_path, &jsonl).unwrap();

    import_from_jsonl(
        &mut storage,
        &jsonl_path,
        &import_config(&beads_dir),
        Some("bd"),
    )
    .unwrap();

    let issues = storage.get_all_issues_for_export().unwrap();
    assert_eq!(issues.len(), 1);
    match &issues[0].issue_type {
        IssueType::Custom(val) => {
            assert_eq!(
                val, "security_audit",
                "custom issue type normalizes to lowercase through DB round-trip"
            );
        }
        other => panic!("Expected Custom issue type, got {:?}", other),
    }
}
