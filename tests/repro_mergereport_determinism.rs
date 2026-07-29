//! Reproduction coverage for deterministic merge report note ordering.
//!
//! The merge CLI prints `MergeReport.notes` in JSON mode. If the report walks a
//! `HashSet` of issue IDs, each `br` process can emit the same semantic merge in
//! a different byte order. This test exercises the CLI surface repeatedly with
//! identical DB+JSONL pairs and compares raw stdout bytes.

mod common;

use beads_rust::model::{Issue, IssueType, Priority, Status};
use chrono::{Duration, TimeZone, Utc};
use common::cli::{BrWorkspace, run_br};
use serde_json::Value;
use std::fs;

const ISSUE_IDS: [&str; 6] = ["bd-c", "bd-a", "bd-f", "bd-b", "bd-e", "bd-d"];
const REPEAT_COUNT: usize = 50;

fn fixed_time(offset_secs: i64) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("fixed timestamp should be valid")
        + Duration::seconds(offset_secs)
}

fn make_issue(id: &str, description: &str, offset_secs: i64) -> Issue {
    let timestamp = fixed_time(offset_secs);
    Issue {
        id: id.to_string(),
        title: format!("Merge determinism {id}"),
        description: Some(description.to_string()),
        status: Status::Open,
        priority: Priority::MEDIUM,
        issue_type: IssueType::Task,
        created_at: timestamp,
        updated_at: timestamp,
        created_by: Some("determinism-test".to_string()),
        source_repo: Some(".".to_string()),
        ..Issue::default()
    }
}

fn issues_jsonl(description_prefix: &str, offset_secs: i64) -> String {
    let mut jsonl = String::new();
    for id in ISSUE_IDS {
        let issue = make_issue(id, &format!("{description_prefix} {id}"), offset_secs);
        jsonl.push_str(&serde_json::to_string(&issue).expect("serialize issue"));
        jsonl.push('\n');
    }
    jsonl
}

fn seed_workspace(workspace: &BrWorkspace) {
    let init = run_br(workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let base_jsonl = issues_jsonl("base", 0);
    fs::write(&jsonl_path, &base_jsonl).expect("write seed jsonl");

    let import = run_br(
        workspace,
        ["--json", "sync", "--import-only", "--force"],
        "import_seed_jsonl",
    );
    assert!(
        import.status.success(),
        "import failed: stdout={} stderr={}",
        import.stdout,
        import.stderr
    );

    fs::write(beads_dir.join("beads.base.jsonl"), base_jsonl).expect("write base snapshot");
}

fn make_local_changes(workspace: &BrWorkspace) {
    for id in ISSUE_IDS {
        let description = format!("local {id}");
        let update = run_br(
            workspace,
            [
                "update",
                id,
                "--description",
                description.as_str(),
                "--no-auto-flush",
            ],
            &format!("local_update_{id}"),
        );
        assert!(
            update.status.success(),
            "local update failed for {id}: stdout={} stderr={}",
            update.stdout,
            update.stderr
        );
    }
}

fn write_external_changes(workspace: &BrWorkspace) {
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    fs::write(jsonl_path, issues_jsonl("external", 100)).expect("write external jsonl");
}

fn run_deterministic_merge_once() -> String {
    let workspace = BrWorkspace::new();
    seed_workspace(&workspace);
    make_local_changes(&workspace);
    write_external_changes(&workspace);

    let merge = run_br(
        &workspace,
        ["--json", "sync", "--merge", "--force-db"],
        "merge_force_db",
    );
    assert!(
        merge.status.success(),
        "merge failed: stdout={} stderr={}",
        merge.stdout,
        merge.stderr
    );

    merge.stdout
}

fn merge_note_ids(output: &str) -> Vec<String> {
    let value: Value = serde_json::from_str(output.trim()).expect("parse merge JSON");
    value
        .get("notes")
        .and_then(Value::as_array)
        .expect("notes should be an array")
        .iter()
        .map(|note| {
            note.as_array()
                .and_then(|pair| pair.first())
                .and_then(Value::as_str)
                .expect("note should start with issue id")
                .to_string()
        })
        .collect()
}

#[test]
fn sync_merge_json_notes_are_byte_identical_across_processes() {
    let expected_note_ids = vec!["bd-a", "bd-b", "bd-c", "bd-d", "bd-e", "bd-f"];
    let baseline = run_deterministic_merge_once();
    assert_eq!(merge_note_ids(&baseline), expected_note_ids);

    for attempt in 1..=REPEAT_COUNT {
        let candidate = run_deterministic_merge_once();
        assert_eq!(
            baseline, candidate,
            "merge JSON output changed on attempt {attempt}"
        );
    }
}
